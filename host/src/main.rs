//! `remvnc-host`: runs `remvnc run` on the tablet over SSH and replays its
//! pen and touch input through local uinput devices.

mod uinput;

use remvnc_proto::Msg;
use std::collections::HashMap;
use std::io::BufReader;
use std::process::{Command, ExitCode, Stdio};

const USAGE: &str = "usage: remvnc-host [--ssh root@10.11.99.1] [--remote /home/root/remvnc]
                   [--deploy <local remvnc binary>] [--rotate 0|90|180|270]
                   [--touch touchpad|touchscreen]";

struct Args {
    ssh: String,
    remote: String,
    deploy: Option<String>,
    run_args: Vec<String>,
}

fn parse() -> Result<Args, String> {
    let mut a = Args { ssh: "root@10.11.99.1".into(), remote: "/home/root/remvnc".into(), deploy: None, run_args: vec![] };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} needs a value\n{USAGE}"));
        match flag.as_str() {
            "--ssh" => a.ssh = value()?,
            "--remote" => a.remote = value()?,
            "--deploy" => a.deploy = Some(value()?),
            "--rotate" | "--touch" => {
                let v = value()?;
                a.run_args.extend([flag, v]);
            }
            "-h" | "--help" => return Err(USAGE.into()),
            _ => return Err(format!("unknown argument {flag}\n{USAGE}")),
        }
    }
    Ok(a)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("remvnc-host: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse()?;
    if let Some(local) = &args.deploy {
        let status = Command::new("scp")
            .args(["-q", "-O", local, &format!("{}:{}", args.ssh, args.remote)])
            .status()
            .map_err(|e| format!("scp: {e}"))?;
        if !status.success() {
            return Err("deploy failed".into());
        }
    }

    let remote_cmd = std::iter::once(format!("{} run", args.remote)).chain(args.run_args.iter().cloned()).collect::<Vec<_>>().join(" ");
    // stdin stays open for the whole session: the tablet exits (and hands
    // the screen back) as soon as it closes.
    let mut child = Command::new("ssh")
        .args(["-T", "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=2", &args.ssh, &remote_cmd])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ssh: {e}"))?;
    let _keep_stdin = child.stdin.take();
    let mut stream = BufReader::new(child.stdout.take().unwrap());

    let mut devices: HashMap<u8, uinput::VirtualDevice> = HashMap::new();
    let result = loop {
        match Msg::read_from(&mut stream) {
            Ok(Some(Msg::Hello { version, width, height, rotation })) => {
                if version != remvnc_proto::VERSION {
                    break Err(format!("tablet speaks protocol {version}, we speak {}", remvnc_proto::VERSION));
                }
                eprintln!("remvnc-host: tablet ready, {width}x{height} rotated {rotation}°");
            }
            Ok(Some(Msg::Device(d))) => match uinput::VirtualDevice::create(&d) {
                Ok(v) => {
                    eprintln!("remvnc-host: created \"{}\"", d.name);
                    devices.insert(d.id, v);
                }
                Err(e) => break Err(format!("creating \"{}\": {e}", d.name)),
            },
            Ok(Some(Msg::Frame { device, events })) => {
                if let Some(v) = devices.get_mut(&device) {
                    if let Err(e) = v.emit(&events) {
                        eprintln!("remvnc-host: write to device {device}: {e}");
                    }
                }
            }
            Ok(Some(Msg::Bye)) => {
                eprintln!("remvnc-host: tablet closed the session");
                break Ok(());
            }
            Ok(None) => break Err("connection closed".into()),
            Err(e) => break Err(format!("protocol: {e}")),
        }
    };
    drop(devices);
    drop(_keep_stdin);
    let _ = child.wait();
    result
}
