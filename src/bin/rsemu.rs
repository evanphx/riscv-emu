//! rsemu: RISC-V system and Linux userspace emulator.
//!
//! The command line follows the Go flag package conventions of the
//! original goemu: flags take a single or double dash, values follow
//! as the next argument or after `=`, and flag parsing stops at the
//! first non-flag argument. Arguments ending in `.goemu` are system
//! configuration files.

use riscv::system::{self, SystemConfig};
use riscv::Params;
use std::collections::HashMap;
use std::process::exit;

struct FlagSpec {
    name: &'static str,
    boolean: bool,
    multi: bool,
    usage: &'static str,
}

const FLAGS: &[FlagSpec] = &[
    FlagSpec { name: "cpuprofile", boolean: false, multi: false, usage: "write cpu profile to `file` (unsupported)" },
    FlagSpec { name: "v", boolean: true, multi: false, usage: "verbose output" },
    FlagSpec { name: "ktrace", boolean: true, multi: false, usage: "kernel trace" },
    FlagSpec { name: "cputrace", boolean: true, multi: false, usage: "CPU trace" },
    FlagSpec { name: "color", boolean: true, multi: false, usage: "turn on color output" },
    FlagSpec { name: "fsroot", boolean: false, multi: false, usage: "filesystem root" },
    FlagSpec { name: "D", boolean: true, multi: false, usage: "disassemble" },
    FlagSpec { name: "bios", boolean: false, multi: false, usage: "the firmware; enables system emulation" },
    FlagSpec { name: "kernel", boolean: false, multi: false, usage: "the kernel; enables system emulation" },
    FlagSpec { name: "initrd", boolean: false, multi: false, usage: "the init filesystem" },
    FlagSpec { name: "append", boolean: false, multi: false, usage: "kernel boot args" },
    FlagSpec { name: "symbols", boolean: false, multi: false, usage: "kernel System.map" },
    FlagSpec { name: "log", boolean: false, multi: false, usage: "logger output file" },
    FlagSpec { name: "cooked", boolean: true, multi: false, usage: "don't enable raw terminal mode" },
    FlagSpec { name: "csr802", boolean: false, multi: false, usage: "CSR802 CPU profiling filename" },
    FlagSpec { name: "gpu", boolean: false, multi: false, usage: "graphics device" },
    FlagSpec { name: "htif", boolean: true, multi: false, usage: "enable host target interface" },
    FlagSpec { name: "dumpdtb", boolean: false, multi: false, usage: "dump DTB to `file`" },
    FlagSpec { name: "m", boolean: false, multi: false, usage: "guest RAM size" },
    FlagSpec { name: "nographic", boolean: true, multi: false, usage: "disable graphical output" },
    FlagSpec { name: "drive", boolean: false, multi: true, usage: "configure drive" },
    FlagSpec { name: "device", boolean: false, multi: true, usage: "configure device" },
    FlagSpec { name: "netdev", boolean: false, multi: true, usage: "configure network device" },
];

fn usage() -> ! {
    eprintln!("Usage of rsemu:");
    for f in FLAGS {
        let arg = if f.boolean { "" } else { " value" };
        eprintln!("  -{}{}\n    \t{}", f.name, arg, f.usage);
    }
    exit(2);
}

struct Flags {
    values: HashMap<&'static str, Vec<String>>,
    args: Vec<String>,
}

impl Flags {
    fn parse(argv: &[String]) -> Flags {
        let mut values: HashMap<&'static str, Vec<String>> = HashMap::new();
        let mut i = 0;
        while i < argv.len() {
            let a = &argv[i];
            if a == "--" {
                i += 1;
                break;
            }
            if !a.starts_with('-') || a == "-" {
                break;
            }
            let body = a.trim_start_matches('-');
            let (name, inline) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (body, None),
            };
            if name == "h" || name == "help" {
                usage();
            }
            let spec = FLAGS.iter().find(|f| f.name == name).unwrap_or_else(|| {
                eprintln!("flag provided but not defined: -{}", name);
                usage();
            });
            let v = if spec.boolean {
                inline.unwrap_or_else(|| "true".to_string())
            } else if let Some(v) = inline {
                v
            } else {
                i += 1;
                match argv.get(i) {
                    Some(v) => v.clone(),
                    None => {
                        eprintln!("flag needs an argument: -{}", name);
                        usage();
                    }
                }
            };
            let entry = values.entry(spec.name).or_default();
            if spec.multi {
                entry.push(v);
            } else {
                *entry = vec![v];
            }
            i += 1;
        }
        Flags {
            values,
            args: argv[i..].to_vec(),
        }
    }

    fn string(&self, name: &str) -> String {
        self.values
            .get(name)
            .and_then(|v| v.last().cloned())
            .unwrap_or_default()
    }

    fn bool(&self, name: &str) -> bool {
        matches!(self.string(name).as_str(), "true" | "1" | "t" | "T" | "TRUE" | "True")
    }

    fn list(&self, name: &str) -> Vec<String> {
        self.values.get(name).cloned().unwrap_or_default()
    }
}

fn fatal(msg: impl std::fmt::Display) -> ! {
    eprintln!("{}", msg);
    exit(1);
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let flags = Flags::parse(&argv);

    let params = Params {
        verbose: flags.bool("v"),
        ktrace: flags.bool("ktrace"),
        cpu_trace: flags.bool("cputrace"),
        csr802: flags.string("csr802"),
        profile: !flags.string("cpuprofile").is_empty(),
        color: flags.bool("color"),
        cooked: flags.bool("cooked"),
        fs_root: flags.string("fsroot"),
    };

    if flags.bool("D") {
        for file in &flags.args {
            match riscv::isa::elf::decode_elf(file) {
                Ok(out) => print!("{}", out),
                Err(e) => fatal(e),
            }
        }
        return;
    }

    let mut arg_cfg = SystemConfig {
        bios: flags.string("bios"),
        kernel: flags.string("kernel"),
        symbols: flags.string("symbols"),
        append: flags.string("append"),
        initrd: flags.string("initrd"),
        dumpdtb: flags.string("dumpdtb"),
        nographic: flags.bool("nographic"),
        ..Default::default()
    };
    for d in flags.list("drive") {
        arg_cfg
            .drives
            .push(system::parse_drive(&d).unwrap_or_else(|e| fatal(e)));
    }
    for d in flags.list("device") {
        arg_cfg
            .devices
            .push(system::parse_device(&d).unwrap_or_else(|e| fatal(e)));
    }
    for d in flags.list("netdev") {
        arg_cfg
            .netdevs
            .push(system::parse_netdev(&d).unwrap_or_else(|e| fatal(e)));
    }

    // Process .goemu configuration files.
    let mut cfg: Option<SystemConfig> = None;
    let mut rest = Vec::new();
    for arg in &flags.args {
        if !arg.ends_with(".goemu") {
            rest.push(arg.clone());
            continue;
        }
        let c = system::read_config(arg)
            .unwrap_or_else(|e| fatal(format!("could not read config {}: {}", arg, e)));
        match cfg.as_mut() {
            Some(cfg) => cfg.merge(c),
            None => cfg = Some(c),
        }
    }
    let mut cfg = match cfg {
        Some(mut c) => {
            c.merge(arg_cfg);
            c
        }
        None => arg_cfg,
    };

    let memsize = flags.string("m");
    if !memsize.is_empty() {
        cfg.memory = memsize;
    }
    if cfg.memory.is_empty() {
        cfg.memory = "512M".to_string();
    }

    let gpu = flags.string("gpu");
    if !gpu.is_empty() {
        let g = system::parse_gpu(&gpu).unwrap_or_else(|e| fatal(e));
        cfg.devices.push(system::Device {
            typ: "virtio-gpu-device".to_string(),
            gpu: g.id.clone(),
            ..Default::default()
        });
        cfg.gpus.push(g);
    }

    if cfg.defined() {
        if let Err(e) = system::system_emulation(flags.bool("htif"), &params, &cfg, &rest) {
            fatal(e);
        }
        return;
    }

    // Linux userspace emulation.
    let envp: Vec<String> = std::env::vars().map(|(k, v)| format!("{}={}", k, v)).collect();
    if flags.args.is_empty() {
        usage();
    }
    let prog = &flags.args[0];
    let mut emu = riscv::emulator::Emulator::new(params.clone())
        .unwrap_or_else(|e| fatal(format!("failed to create emulator: {}", e)));
    if let Err(e) = emu.load_elf(prog) {
        fatal(format!("failed to load {}: {}", prog, e));
    }
    match emu.run(&flags.args, &envp) {
        Ok(code) => exit(code),
        Err(e) => {
            println!("run: {}", e);
        }
    }
}
