//! System configuration: `.goemu` JSON files and command line
//! arguments.

use serde::{Deserialize, Serialize};

/// System emulation configuration.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemConfig {
    pub bios: String,
    pub kernel: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub symbols: String,
    pub append: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub initrd: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub dumpdtb: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub memory: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub nographic: bool,
    pub drives: Vec<Drive>,
    pub netdevs: Vec<Netdev>,
    pub gpus: Vec<Gpu>,
    pub devices: Vec<Device>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Drive {
    pub id: String,
    pub file: String,
    pub format: String,
    pub readonly: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Netdev {
    pub id: String,
    pub mac: String,
    pub ip: String,
    pub gw: String,
    pub hostname: String,
    pub domainname: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Gpu {
    pub id: String,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub pointer: String,
}

/// Pointer device types of the GPU input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerType {
    Mouse,
    Touchpad,
}

impl Gpu {
    pub fn pointer_type(&self) -> Result<PointerType, String> {
        match self.pointer.as_str() {
            "abs" | "absolute" | "touchpad" => Ok(PointerType::Touchpad),
            "rel" | "relative" | "mouse" | "" => Ok(PointerType::Mouse),
            p => Err(format!("invalid pointer type '{}'", p)),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Device {
    #[serde(rename = "type")]
    pub typ: String,
    pub drive: String,
    pub netdev: String,
    pub gpu: String,
    pub id: String,
    pub serial: String,
}

fn parse_bool(v: &str) -> Result<bool, String> {
    match v {
        "on" | "true" => Ok(true),
        "off" | "false" => Ok(false),
        _ => Err(format!("invalid boolean value '{}'", v)),
    }
}

/// Parses a memory size such as 512M or 2G.
pub fn parse_mem(v: &str) -> Result<u64, String> {
    let err = || format!("invalid memory specification: {}", v);
    let (digits, suffix) = match v.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => (&v[..i], &v[i..]),
        None => (v, ""),
    };
    if digits.is_empty() || suffix.chars().count() > 1 {
        return Err(err());
    }
    let base: u64 = digits.parse().map_err(|_| err())?;
    let mult = match suffix {
        "" => 1,
        "k" | "K" => 1024,
        "m" | "M" => 1024 * 1024,
        "g" | "G" => 1024 * 1024 * 1024,
        s if s.chars().all(|c| c.is_ascii_alphabetic()) => {
            return Err(format!("invalid suffix '{}'", s))
        }
        _ => return Err(err()),
    };
    Ok(base * mult)
}

fn split_kv(arg: &str, what: &str) -> Result<(String, String), String> {
    let parts: Vec<&str> = arg.split('=').collect();
    if parts.len() != 2 {
        return Err(format!("invalid {} argument: {}", what, arg));
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

/// Parses the -drive argument: id=hd0,file=disk.img,format=raw.
pub fn parse_drive(args: &str) -> Result<Drive, String> {
    let mut drive = Drive::default();
    for arg in args.split(',') {
        let (k, v) = split_kv(arg, "drive")?;
        match k.as_str() {
            "id" => drive.id = v,
            "file" => drive.file = v,
            "format" => drive.format = v,
            "readonly" => {
                drive.readonly =
                    parse_bool(&v).map_err(|_| format!("invalid boolean argument: {}", arg))?
            }
            _ => return Err(format!("invalid argument: {}", arg)),
        }
    }
    Ok(drive)
}

/// Parses the -gpu argument: id=gpu0,width=1024,height=768.
pub fn parse_gpu(args: &str) -> Result<Gpu, String> {
    let mut gpu = Gpu::default();
    for arg in args.split(',') {
        let (k, v) = split_kv(arg, "gpu")?;
        match k.as_str() {
            "title" => gpu.title = v,
            "id" => gpu.id = v,
            "width" => {
                gpu.width = v
                    .parse()
                    .map_err(|e| format!("invalid width {}: {}", v, e))?
            }
            "height" => {
                gpu.height = v
                    .parse()
                    .map_err(|e| format!("invalid height {}: {}", v, e))?
            }
            "pointer" => gpu.pointer = v,
            _ => return Err(format!("invalid argument: {}", arg)),
        }
    }
    Ok(gpu)
}

/// Parses the -device argument: virtio-blk-device,drive=hd0.
pub fn parse_device(args: &str) -> Result<Device, String> {
    let mut dev = Device::default();
    for (idx, arg) in args.split(',').enumerate() {
        if idx == 0 {
            if arg.contains('=') {
                return Err(format!("invalid device type: {}", arg));
            }
            dev.typ = arg.to_string();
            continue;
        }
        let (k, v) = split_kv(arg, "device")?;
        match k.as_str() {
            "drive" => dev.drive = v,
            "netdev" => dev.netdev = v,
            "gpu" => dev.gpu = v,
            "id" => dev.id = v,
            "serial" => dev.serial = v,
            _ => return Err(format!("invalid argument: {}", arg)),
        }
    }
    Ok(dev)
}

/// Parses the -netdev argument: id=eth0,ip=192.168.42.4,gw=192.168.42.254.
pub fn parse_netdev(args: &str) -> Result<Netdev, String> {
    let mut n = Netdev::default();
    for arg in args.split(',') {
        let (k, v) = split_kv(arg, "netdev")?;
        match k.as_str() {
            "id" => n.id = v,
            "mac" => n.mac = v,
            "ip" => n.ip = v,
            "gw" => n.gw = v,
            "hostname" => n.hostname = v,
            "domainname" => n.domainname = v,
            _ => return Err(format!("invalid argument: {}", arg)),
        }
    }
    Ok(n)
}

/// Reads the JSON configuration file.
pub fn read_config(file: &str) -> Result<SystemConfig, String> {
    let data = std::fs::read(file).map_err(|e| format!("{}: {}", file, e))?;
    serde_json::from_slice(&data).map_err(|e| format!("{}: {}", file, e))
}

impl SystemConfig {
    /// Tests if the configuration enables system emulation.
    pub fn defined(&self) -> bool {
        !self.kernel.is_empty() || !self.bios.is_empty()
    }

    /// Merges the configuration o into this configuration. Non-empty
    /// values of o override the values of self and lists are
    /// concatenated.
    pub fn merge(&mut self, o: SystemConfig) {
        fn set(dst: &mut String, src: String) {
            if !src.is_empty() {
                *dst = src;
            }
        }
        set(&mut self.bios, o.bios);
        set(&mut self.kernel, o.kernel);
        set(&mut self.symbols, o.symbols);
        set(&mut self.append, o.append);
        set(&mut self.initrd, o.initrd);
        set(&mut self.dumpdtb, o.dumpdtb);
        set(&mut self.memory, o.memory);
        if o.nographic {
            self.nographic = true;
        }
        self.drives.extend(o.drives);
        self.netdevs.extend(o.netdevs);
        self.gpus.extend(o.gpus);
        self.devices.extend(o.devices);
    }

    pub fn drive(&self, id: &str) -> Option<&Drive> {
        self.drives.iter().find(|d| d.id == id)
    }

    pub fn netdev(&self, id: &str) -> Option<&Netdev> {
        self.netdevs.iter().find(|d| d.id == id)
    }

    pub fn gpu(&self, id: &str) -> Option<&Gpu> {
        self.gpus.iter().find(|d| d.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem() {
        assert_eq!(parse_mem("512M"), Ok(512 << 20));
        assert_eq!(parse_mem("2G"), Ok(2 << 30));
        assert_eq!(parse_mem("4096"), Ok(4096));
        assert_eq!(parse_mem("64k"), Ok(64 << 10));
        assert!(parse_mem("1T").is_err());
        assert!(parse_mem("M").is_err());
        assert!(parse_mem("12MB").is_err());
    }

    #[test]
    fn config_merge() {
        let json = r#"{
          "bios": "opensbi/fw_jump.bin",
          "kernel": "linux/Image",
          "append": "console=ttyS0",
          "drives": [{"id": "hd0", "format": "raw", "file": "rootfs.ext2", "readonly": true}],
          "devices": [{"type": "virtio-blk-device", "drive": "hd0"}]
        }"#;
        let mut cfg: SystemConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.defined());
        assert!(cfg.drive("hd0").unwrap().readonly);
        let mut args = SystemConfig {
            append: "root=/dev/vda".into(),
            ..Default::default()
        };
        args.devices
            .push(parse_device("virtio-blk-device,drive=hd1").unwrap());
        cfg.merge(args);
        assert_eq!(cfg.append, "root=/dev/vda");
        assert_eq!(cfg.kernel, "linux/Image");
        assert_eq!(cfg.devices.len(), 2);
        assert_eq!(cfg.devices[1].drive, "hd1");
    }

    #[test]
    fn drive_and_device_args() {
        let d = parse_drive("id=hd0,format=raw,file=a.img,readonly=on").unwrap();
        assert_eq!(d.id, "hd0");
        assert_eq!(d.file, "a.img");
        assert!(d.readonly);
        assert!(parse_drive("id").is_err());
        assert!(parse_drive("bogus=1").is_err());
        assert!(parse_device("type=x").is_err());
    }
}
