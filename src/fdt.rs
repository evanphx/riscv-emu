//! Flattened Device Tree (FDT) blob builder.

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_VERSION: u32 = 17;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_END: u32 = 9;

/// FDT builder.
#[derive(Default)]
pub struct Fdt {
    structure: Vec<u8>,
    strings: Vec<u8>,
    open_nodes: i32,
}

impl Fdt {
    pub fn new() -> Fdt {
        Fdt::default()
    }

    fn put32(&mut self, v: u32) {
        self.structure.extend_from_slice(&v.to_be_bytes());
    }

    fn put_data(&mut self, data: &[u8]) {
        self.structure.extend_from_slice(data);
        while self.structure.len() % 4 != 0 {
            self.structure.push(0);
        }
    }

    pub fn begin_node(&mut self, name: &str) {
        self.put32(FDT_BEGIN_NODE);
        let mut d = name.as_bytes().to_vec();
        d.push(0);
        self.put_data(&d);
        self.open_nodes += 1;
    }

    /// Begins the node `name@n` with the unit address in hex.
    pub fn begin_node_num(&mut self, name: &str, n: u64) {
        self.begin_node(&format!("{}@{:x}", name, n));
    }

    pub fn end_node(&mut self) {
        self.put32(FDT_END_NODE);
        self.open_nodes -= 1;
    }

    fn string_offset(&mut self, name: &str) -> u32 {
        // Find an existing NUL-terminated entry.
        let mut needle = name.as_bytes().to_vec();
        needle.push(0);
        let mut start = 0;
        while start < self.strings.len() {
            let end = start + self.strings[start..].iter().position(|&b| b == 0).unwrap();
            if self.strings[start..=end] == needle[..] {
                return start as u32;
            }
            start = end + 1;
        }
        let pos = self.strings.len();
        self.strings.extend_from_slice(&needle);
        pos as u32
    }

    /// Adds a property with raw data.
    pub fn prop(&mut self, name: &str, data: &[u8]) {
        self.put32(FDT_PROP);
        self.put32(data.len() as u32);
        let ofs = self.string_offset(name);
        self.put32(ofs);
        self.put_data(data);
    }

    /// Adds an empty property.
    pub fn prop_empty(&mut self, name: &str) {
        self.prop(name, &[]);
    }

    pub fn prop_u32s(&mut self, name: &str, vals: &[u32]) {
        let data: Vec<u8> = vals.iter().flat_map(|v| v.to_be_bytes()).collect();
        self.prop(name, &data);
    }

    pub fn prop_u32(&mut self, name: &str, v: u32) {
        self.prop_u32s(name, &[v]);
    }

    pub fn prop_u64(&mut self, name: &str, v: u64) {
        self.prop_u32s(name, &[(v >> 32) as u32, v as u32]);
    }

    pub fn prop_u64_pair(&mut self, name: &str, a: u64, b: u64) {
        self.prop_u32s(
            name,
            &[(a >> 32) as u32, a as u32, (b >> 32) as u32, b as u32],
        );
    }

    pub fn prop_str(&mut self, name: &str, s: &str) {
        let mut d = s.as_bytes().to_vec();
        d.push(0);
        self.prop(name, &d);
    }

    pub fn prop_strs(&mut self, name: &str, strs: &[&str]) {
        let mut d = Vec::new();
        for s in strs {
            d.extend_from_slice(s.as_bytes());
            d.push(0);
        }
        self.prop(name, &d);
    }

    /// Finishes the tree and returns the DTB blob.
    pub fn output(mut self) -> Vec<u8> {
        assert_eq!(self.open_nodes, 0, "open nodes in FDT");
        self.put32(FDT_END);

        let header_size = 40usize;
        let rsvmap_off = (header_size + 7) & !7;
        let struct_off = rsvmap_off + 16;
        let strings_off = struct_off + self.structure.len();
        let total = strings_off + self.strings.len();

        let mut out = Vec::with_capacity(total);
        let header = [
            FDT_MAGIC,
            total as u32,
            struct_off as u32,
            strings_off as u32,
            rsvmap_off as u32,
            FDT_VERSION,
            16,
            0,
            self.strings.len() as u32,
            self.structure.len() as u32,
        ];
        for h in header {
            out.extend_from_slice(&h.to_be_bytes());
        }
        out.resize(rsvmap_off, 0);
        out.extend_from_slice(&[0u8; 16]);
        out.extend_from_slice(&self.structure);
        out.extend_from_slice(&self.strings);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_table_dedup() {
        let mut f = Fdt::new();
        f.begin_node("");
        f.prop_u32("regmap", 3);
        f.prop_u32("reg", 1);
        f.prop_u32("map", 2);
        f.prop_u32("reg", 4);
        f.end_node();
        let blob = f.output();
        let strings_off = u32::from_be_bytes(blob[12..16].try_into().unwrap()) as usize;
        assert_eq!(&blob[strings_off..], b"regmap\0reg\0map\0");
        assert_eq!(
            u32::from_be_bytes(blob[0..4].try_into().unwrap()),
            0xd00dfeed
        );
        assert_eq!(
            u32::from_be_bytes(blob[4..8].try_into().unwrap()) as usize,
            blob.len()
        );
    }
}
