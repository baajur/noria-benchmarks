extern crate chrono;
#[macro_use]
extern crate clap;
extern crate failure;
extern crate rusoto_core;
extern crate rusoto_sts;
extern crate tsunami;

use clap::{App, Arg};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::prelude::*;
use std::io::BufReader;
use std::{fmt, thread, time};
use tsunami::*;

const AMI: &str = "ami-7342f90c";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Backend {
    Mysql,
    Soup,
    Soupy,
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match *self {
            Backend::Mysql => write!(f, "mysql"),
            Backend::Soup => write!(f, "soup"),
            Backend::Soupy => write!(f, "soupy"),
        }
    }
}

fn git_and_cargo(
    ssh: &mut Session,
    dir: &str,
    bin: &str,
    branch: Option<&str>,
) -> Result<(), failure::Error> {
    let branch = branch.unwrap_or("master");
    if dir != "distributary" {
        eprintln!(" -> git fetch && reset");
        ssh.cmd(&format!(
            "bash -c 'git -C {} fetch && git -C {} reset --hard origin/{} 2>&1'",
            dir, dir, branch
        )).map(|out| {
            let out = out.trim_right();
            if !out.is_empty() && !out.contains("Already up-to-date.") {
                eprintln!("{}", out);
            }
        })?;
    }

    eprintln!(" -> rebuild");
    ssh.cmd(&format!(
        "bash -c 'cd {} && cargo b --release --bin {} 2>&1'",
        dir, bin
    )).map(|out| {
            let out = out.trim_right();
            if !out.is_empty() {
                eprintln!("{}", out);
            }
        })
        .map_err(|e| {
            eprintln!(" -> rebuild failed!\n{:?}", e);
            e
        })?;

    Ok(())
}

fn main() {
    let args = App::new("trawler-mysql ec2 orchestrator")
        .about("Run the MySQL trawler benchmark on ec2")
        .arg(
            Arg::with_name("memory_limit")
                .takes_value(true)
                .long("memory-limit")
                .help("Partial state size limit / eviction threshold [in bytes]."),
        )
        .arg(
            Arg::with_name("memscale")
                .takes_value(true)
                .default_value("1")
                .long("memscale")
                .help("Memscale to use [default: 1]."),
        )
        .arg(
            Arg::with_name("SCALE")
                .help("Run the given scale(s).")
                .multiple(true),
        )
        .get_matches();

    let mut b = TsunamiBuilder::default();
    b.use_term_logger();
    b.add_set(
        "trawler",
        1,
        MachineSetup::new("m5.12xlarge", AMI, |ssh| {
            eprintln!("==> setting up trawler");
            git_and_cargo(ssh, "benchmarks/lobsters/mysql", "trawler-mysql", None)?;
            eprintln!("==> setting up trawler w/ soup hacks");
            git_and_cargo(
                ssh,
                "benchmarks-soup/lobsters/mysql",
                "trawler-mysql",
                Some("hacks-for-soup-evict"),
            )?;
            eprintln!("==> setting up trawler w/ soupy");
            git_and_cargo(
                ssh,
                "benchmarks-soupy/lobsters/mysql",
                "trawler-mysql",
                Some("soupy-evict"),
            )?;
            eprintln!("==> setting up shim");
            git_and_cargo(ssh, "shim", "distributary-mysql", None)?;
            Ok(())
        }).as_user("ubuntu"),
    );
    b.add_set(
        "server",
        1,
        MachineSetup::new("c5.4xlarge", AMI, |ssh| {
            eprintln!("==> setting up souplet");
            git_and_cargo(ssh, "distributary", "souplet", None)?;
            eprintln!("==> setting up zk-util");
            git_and_cargo(ssh, "distributary/consensus", "zk-util", None)?;
            Ok(())
        }).as_user("ubuntu"),
    );

    // https://github.com/rusoto/rusoto/blob/master/AWS-CREDENTIALS.md
    let sts = rusoto_sts::StsClient::new(
        rusoto_core::default_tls_client().unwrap(),
        rusoto_core::EnvironmentProvider,
        rusoto_core::Region::UsEast1,
    );
    let provider = rusoto_sts::StsAssumeRoleSessionCredentialsProvider::new(
        sts,
        "arn:aws:sts::125163634912:role/soup".to_owned(),
        "vote-benchmark".to_owned(),
        None,
        None,
        None,
        None,
    );

    b.set_max_duration(5);
    b.set_region(rusoto_core::Region::UsEast1);
    b.wait_limit(time::Duration::from_secs(60));

    let scales: Box<Iterator<Item = usize>> = args.values_of("SCALE")
        .map(|it| Box::new(it.map(|s| s.parse().unwrap())) as Box<_>)
        .unwrap_or(Box::new(
            [
                //100, 200, 400, 800, 1000usize, 1250, 1500, 2000, 3000, 4000, 4500, 5000, 5500,
                //6000, 6500, 7000, 8000, 8500, 9000, 9500, 10_000,
                100usize,
                400,
                800,
                1000,
                1250,
                1500,
                2000,
                2500,
                3000,
                3500,
                4000,
                4500,
                5000,
                5500,
                6000,
                6500,
                7000,
                7500,
                8000,
                8500,
                9000,
                9500,
                10_000,
            ].into_iter()
                .map(|&s| s),
        ) as Box<_>);

    let memscale = value_t_or_exit!(args, "memscale", usize);
    let memlimit = args.value_of("memory_limit");

    let mut load = if args.is_present("SCALE") {
        OpenOptions::new()
            .write(true)
            .truncate(false)
            .append(true)
            .create(true)
            .open("load.log")
            .unwrap()
    } else {
        let mut f = File::create("load.log").unwrap();
        f.write_all(b"#reqscale backend sload1 sload5 cload1 cload5\n")
            .unwrap();
        f
    };
    b.run_as(provider, |mut vms: HashMap<String, Vec<Machine>>| {
        use chrono::prelude::*;

        let mut server = vms.remove("server").unwrap().swap_remove(0);
        let mut trawler = vms.remove("trawler").unwrap().swap_remove(0);

        let backends = [Backend::Mysql, Backend::Soup, Backend::Soupy];
        let mut survived_last: HashMap<_, _> = backends.iter().map(|b| (b, true)).collect();

        // allow reuse of time-wait ports
        trawler
            .ssh
            .as_mut()
            .unwrap()
            .cmd("bash -c 'echo 1 | sudo tee /proc/sys/net/ipv4/tcp_tw_reuse'")?;

        for scale in scales {
            for backend in &backends {
                if !survived_last[backend] {
                    continue;
                }

                eprintln!("==> benchmark {} w/ {}x load", backend, scale);

                match backend {
                    Backend::Mysql => {
                        let ssh = server.ssh.as_mut().unwrap();
                        ssh.cmd("sudo mount -t tmpfs -o size=16G tmpfs /mnt")?;
                        // sudo rm -rf /var/lib/mysql
                        ssh.cmd("sudo cp -r /var/lib/mysql.clean /mnt/mysql")?;
                        // sudo ln -s /mnt/mysql /var/lib/mysql
                        ssh.cmd("sudo chown -R mysql:mysql /var/lib/mysql/")?;
                    }
                    Backend::Soup | Backend::Soupy => {
                        // just to make totally sure
                        server
                            .ssh
                            .as_mut()
                            .unwrap()
                            .cmd("bash -c 'pkill -9 -f souplet 2>&1'")
                            .map(|out| {
                                let out = out.trim_right();
                                if !out.is_empty() {
                                    eprintln!(" -> force stopped soup...\n{}", out);
                                }
                            })?;
                        trawler
                            .ssh
                            .as_mut()
                            .unwrap()
                            .cmd("bash -c 'pkill -9 -f distributary-mysql 2>&1'")
                            .map(|out| {
                                let out = out.trim_right();
                                if !out.is_empty() {
                                    eprintln!(" -> force stopped shim...\n{}", out);
                                }
                            })?;

                        // XXX: also delete log files if we later run with RocksDB?
                        server
                            .ssh
                            .as_mut()
                            .unwrap()
                            .cmd(
                                "distributary/target/release/zk-util \
                                 --clean --deployment trawler",
                            )
                            .map(|out| {
                                let out = out.trim_right();
                                if !out.is_empty() {
                                    eprintln!(" -> wiped soup state...\n{}", out);
                                }
                            })?;
                        // Don't hit Soup listening timeout think
                        thread::sleep(time::Duration::from_secs(10));
                    }
                }

                // start server again
                match backend {
                    Backend::Mysql => server
                        .ssh
                        .as_mut()
                        .unwrap()
                        .cmd("bash -c 'sudo systemctl start mysql 2>&1'")
                        .map(|out| {
                            let out = out.trim_right();
                            if !out.is_empty() {
                                eprintln!(" -> started mysql...\n{}", out);
                            }
                        })?,
                    Backend::Soup | Backend::Soupy => {
                        let mut cmd = format!(
                            "bash -c 'nohup \
                             env RUST_BACKTRACE=1 \
                             distributary/target/release/souplet \
                             --deployment trawler \
                             --durability memory \
                             --no-reuse \
                             --address {} \
                             --readers 60 -w 5 \
                             --shards 0 ",
                            server.private_ip
                        );
                        if let Some(memlimit) = memlimit {
                            cmd.push_str(&format!("--memory {} ", memlimit));
                        }
                        cmd.push_str(" &> souplet.log &'");

                        server.ssh.as_mut().unwrap().cmd(&cmd).map(|_| ())?;

                        // start the shim (which will block until soup is available)
                        trawler
                            .ssh
                            .as_mut()
                            .unwrap()
                            .cmd(&format!(
                                "bash -c 'nohup \
                                 env RUST_BACKTRACE=1 \
                                 shim/target/release/distributary-mysql \
                                 --deployment trawler \
                                 --no-sanitize --no-static-responses \
                                 -z {}:2181 \
                                 -p 3306 \
                                 &> shim.log &'",
                                server.private_ip,
                            ))
                            .map(|_| ())?;

                        // give soup a chance to start
                        thread::sleep(time::Duration::from_secs(5));
                    }
                }

                // run priming
                // XXX: with MySQL we *could* just reprime by copying over the old ramdisk again
                eprintln!(" -> priming at {}", Local::now().time().format("%H:%M:%S"));

                let dir = match backend {
                    Backend::Mysql => "benchmarks",
                    Backend::Soup => "benchmarks-soup",
                    Backend::Soupy => "benchmarks-soupy",
                };

                let ip = match backend {
                    Backend::Mysql => &*server.private_ip,
                    Backend::Soup | Backend::Soupy => "127.0.0.1",
                };

                trawler
                    .ssh
                    .as_mut()
                    .unwrap()
                    .cmd(&format!(
                        "env RUST_BACKTRACE=1 \
                         {}/lobsters/mysql/target/release/trawler-mysql \
                         --memscale {} \
                         --warmup 0 \
                         --runtime 0 \
                         --issuers 24 \
                         --prime \
                         \"mysql://lobsters:$(cat ~/mysql.pass)@{}/lobsters\"",
                        dir, memscale, ip
                    ))
                    .map(|out| {
                        let out = out.trim_right();
                        if !out.is_empty() {
                            eprintln!(" -> priming finished...\n{}", out);
                        }
                    })?;

                if memlimit.is_some() {
                    eprintln!(
                        " -> running uniform at {}",
                        Local::now().time().format("%H:%M:%S")
                    );

                    trawler
                        .ssh
                        .as_mut()
                        .unwrap()
                        .cmd(&format!(
                            "env RUST_BACKTRACE=1 \
                             {}/lobsters/mysql/target/release/trawler-mysql \
                             --uniform \
                             --reqscale 3000 \
                             --memscale {} \
                             --warmup 300 \
                             --runtime 0 \
                             --issuers 24 \
                             \"mysql://lobsters:$(cat ~/mysql.pass)@{}/lobsters\"",
                            dir, memscale, ip
                        ))
                        .map(|out| {
                            let out = out.trim_right();
                            if !out.is_empty() {
                                eprintln!(" -> uniform finished...\n{}", out);
                            }
                        })?;
                }

                eprintln!(" -> warming at {}", Local::now().time().format("%H:%M:%S"));

                trawler
                    .ssh
                    .as_mut()
                    .unwrap()
                    .cmd(&format!(
                        "env RUST_BACKTRACE=1 \
                         {}/lobsters/mysql/target/release/trawler-mysql \
                         --reqscale 3000 \
                         --memscale {} \
                         --warmup 120 \
                         --runtime 0 \
                         --issuers 24 \
                         \"mysql://lobsters:$(cat ~/mysql.pass)@{}/lobsters\"",
                        dir, memscale, ip
                    ))
                    .map(|out| {
                        let out = out.trim_right();
                        if !out.is_empty() {
                            eprintln!(" -> warming finished...\n{}", out);
                        }
                    })?;

                eprintln!(" -> started at {}", Local::now().time().format("%H:%M:%S"));

                let prefix = format!("lobsters-{}-{}", backend, scale);
                let mut output = File::create(format!("{}.log", prefix))?;
                let hist_output = if let Some(memlimit) = memlimit {
                    format!(
                        "--histogram lobsters-{}-m{}-r{}-l{}.hist ",
                        backend, memscale, scale, memlimit
                    )
                } else {
                    format!(
                        "--histogram lobsters-{}-m{}-r{}-unlimited.hist ",
                        backend, memscale, scale
                    )
                };
                trawler
                    .ssh
                    .as_mut()
                    .unwrap()
                    .cmd_raw(&format!(
                        "env RUST_BACKTRACE=1 \
                         {}/lobsters/mysql/target/release/trawler-mysql \
                         --reqscale {} \
                         --memscale {} \
                         --warmup 20 \
                         --runtime 30 \
                         --issuers 24 \
                         {}
                         \"mysql://lobsters:$(cat ~/mysql.pass)@{}/lobsters\"",
                        dir, scale, memscale, hist_output, ip
                    ))
                    .and_then(|out| Ok(output.write_all(&out[..]).map(|_| ())?))?;

                drop(output);
                eprintln!(" -> finished at {}", Local::now().time().format("%H:%M:%S"));

                // gather server load
                let sload = server
                    .ssh
                    .as_mut()
                    .unwrap()
                    .cmd("awk '{print $1\" \"$2}' /proc/loadavg")?;
                let sload = sload.trim_right();

                // gather client load
                let cload = trawler
                    .ssh
                    .as_mut()
                    .unwrap()
                    .cmd("awk '{print $1\" \"$2}' /proc/loadavg")?;
                let cload = cload.trim_right();

                load.write_all(format!("{} {} ", scale, backend).as_bytes())?;
                load.write_all(sload.as_bytes())?;
                load.write_all(b" ")?;
                load.write_all(cload.as_bytes())?;
                load.write_all(b"\n")?;

                let mut hist = File::create(format!("{}.hist", prefix))?;
                let hist_cmd = if let Some(memlimit) = memlimit {
                    format!(
                        "cat lobsters-{}-m{}-r{}-l{}.hist",
                        backend, memscale, scale, memlimit
                    )
                } else {
                    format!(
                        "cat lobsters-{}-m{}-r{}-unlimited.hist",
                        backend, memscale, scale
                    )
                };
                trawler
                    .ssh
                    .as_mut()
                    .unwrap()
                    .cmd_raw(&hist_cmd)
                    .and_then(|out| Ok(hist.write_all(&out[..]).map(|_| ())?))?;

                // stop old server
                match backend {
                    Backend::Mysql => {
                        server
                            .ssh
                            .as_mut()
                            .unwrap()
                            .cmd("bash -c 'sudo systemctl stop mysql 2>&1'")
                            .map(|out| {
                                let out = out.trim_right();
                                if !out.is_empty() {
                                    eprintln!(" -> stopped mysql...\n{}", out);
                                }
                            })?;
                        server.ssh.as_mut().unwrap().cmd("sudo umount /mnt")?;
                    }
                    Backend::Soup | Backend::Soupy => {
                        // gather state size
                        let mem_limit = if let Some(limit) = memlimit {
                            format!("l{}", limit)
                        } else {
                            "unlimited".to_owned()
                        };
                        let mut sizefile = File::create(format!(
                            "lobsters-{}-m{}-r{}-{}.json",
                            backend, memscale, scale, mem_limit
                        ))?;
                        trawler
                            .ssh
                            .as_mut()
                            .unwrap()
                            .cmd_raw(&format!(
                                "wget http://{}:9000/get_statistics",
                                server.private_ip
                            ))
                            .and_then(|out| Ok(sizefile.write_all(&out[..]).map(|_| ())?))?;

                        server
                            .ssh
                            .as_mut()
                            .unwrap()
                            .cmd("bash -c 'pkill -f souplet 2>&1'")
                            .map(|out| {
                                let out = out.trim_right();
                                if !out.is_empty() {
                                    eprintln!(" -> stopped soup...\n{}", out);
                                }
                            })?;
                        trawler
                            .ssh
                            .as_mut()
                            .unwrap()
                            .cmd("bash -c 'pkill -f distributary-mysql 2>&1'")
                            .map(|out| {
                                let out = out.trim_right();
                                if !out.is_empty() {
                                    eprintln!(" -> stopped shim...\n{}", out);
                                }
                            })?;

                        // give it some time
                        thread::sleep(time::Duration::from_secs(2));
                    }
                }

                // stop iterating through scales for this backend if it's not keeping up
                let sload: f64 = sload
                    .split_whitespace()
                    .next()
                    .and_then(|l| l.parse().ok())
                    .unwrap_or(0.0);
                let cload: f64 = cload
                    .split_whitespace()
                    .next()
                    .and_then(|l| l.parse().ok())
                    .unwrap_or(0.0);

                eprintln!(" -> backend load: s: {}/16, c: {}/48", sload, cload);

                if sload > 16.5 {
                    eprintln!(" -> backend is probably not keeping up");
                    //*survived_last.get_mut(backend).unwrap() = false;
                }

                // also parse achived ops/s to check that we're *really* keeping up
                let log = File::open(format!("{}.log", prefix))?;
                let log = BufReader::new(log);
                let mut target = None;
                let mut actual = None;
                for line in log.lines() {
                    let line = line?;
                    if line.starts_with("# target ops/s") {
                        target = Some(line.rsplitn(2, ' ').next().unwrap().parse::<f64>()?);
                    } else if line.starts_with("# achieved ops/s") {
                        actual = Some(line.rsplitn(2, ' ').next().unwrap().parse::<f64>()?);
                    }
                    match (target, actual) {
                        (Some(target), Some(actual)) => {
                            eprintln!(" -> achieved {} ops/s (target: {})", actual, target);
                            if actual < target * 3.0 / 4.0 {
                                eprintln!(" -> backend is really not keeping up");
                                *survived_last.get_mut(backend).unwrap() = false;
                            }
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }).unwrap();
}
