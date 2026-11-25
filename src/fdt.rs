//! Flattened device tree (DTB) writer, from the devicetree spec, chapter 5.
//!
//! On arm64 there is no PCI enumeration or BIOS table walk at boot: Linux
//! learns what hardware exists (RAM, CPUs, interrupt controller, timer, UART)
//! from this blob, whose address we hand it in register x0.
//!
//! Layout: [header 40 B][memory reservation map][structure block][strings].
//! The structure block is a token stream: BEGIN_NODE name, PROP len nameoff
//! value, END_NODE, ..., END. Property names live once in the strings block.
//! Everything is big-endian and 4-byte aligned.

use std::collections::HashMap;

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_END: u32 = 9;

#[derive(Default)]
pub struct Fdt {
    structure: Vec<u8>,
    strings: Vec<u8>,
    string_offsets: HashMap<String, u32>,
    depth: usize,
}

fn pad4(buf: &mut Vec<u8>) {
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
}

impl Fdt {
    pub fn new() -> Self {
        Self::default()
    }

    fn token(&mut self, t: u32) {
        self.structure.extend_from_slice(&t.to_be_bytes());
    }

    pub fn begin_node(&mut self, name: &str) {
        self.token(FDT_BEGIN_NODE);
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        pad4(&mut self.structure);
        self.depth += 1;
    }

    pub fn end_node(&mut self) {
        self.token(FDT_END_NODE);
        self.depth -= 1;
    }

    pub fn prop(&mut self, name: &str, value: &[u8]) {
        let off = match self.string_offsets.get(name) {
            Some(&off) => off,
            None => {
                let off = self.strings.len() as u32;
                self.strings.extend_from_slice(name.as_bytes());
                self.strings.push(0);
                self.string_offsets.insert(name.to_string(), off);
                off
            }
        };
        self.token(FDT_PROP);
        self.structure.extend_from_slice(&(value.len() as u32).to_be_bytes());
        self.structure.extend_from_slice(&off.to_be_bytes());
        self.structure.extend_from_slice(value);
        pad4(&mut self.structure);
    }

    /// Boolean property: present = true.
    pub fn prop_empty(&mut self, name: &str) {
        self.prop(name, &[]);
    }

    pub fn prop_u32(&mut self, name: &str, v: u32) {
        self.prop(name, &v.to_be_bytes());
    }

    pub fn prop_cells(&mut self, name: &str, cells: &[u32]) {
        let bytes: Vec<u8> = cells.iter().flat_map(|c| c.to_be_bytes()).collect();
        self.prop(name, &bytes);
    }

    pub fn prop_u64(&mut self, name: &str, v: u64) {
        self.prop(name, &v.to_be_bytes());
    }

    /// `reg`-style list of (address, size) pairs with 2 cells each.
    pub fn prop_regs(&mut self, name: &str, regs: &[(u64, u64)]) {
        let bytes: Vec<u8> = regs.iter().flat_map(|(a, s)| [a.to_be_bytes(), s.to_be_bytes()].concat()).collect();
        self.prop(name, &bytes);
    }

    pub fn prop_str(&mut self, name: &str, s: &str) {
        self.prop_strs(name, &[s]);
    }

    /// String list: each string NUL-terminated, back to back.
    pub fn prop_strs(&mut self, name: &str, ss: &[&str]) {
        let mut bytes = Vec::new();
        for s in ss {
            bytes.extend_from_slice(s.as_bytes());
            bytes.push(0);
        }
        self.prop(name, &bytes);
    }

    pub fn finish(mut self) -> Vec<u8> {
        assert_eq!(self.depth, 0, "unbalanced begin_node/end_node");
        self.token(FDT_END);
        let header_len = 40;
        let rsvmap_len = 16; // one all-zero entry terminates the (empty) map
        let off_struct = header_len + rsvmap_len;
        let off_strings = off_struct + self.structure.len();
        let total = off_strings + self.strings.len();
        let header = [
            FDT_MAGIC,
            total as u32,
            off_struct as u32,
            off_strings as u32,
            header_len as u32, // off_mem_rsvmap
            17,                // version
            16,                // last compatible version
            0,                 // boot CPU id
            self.strings.len() as u32,
            self.structure.len() as u32,
        ];
        let mut out: Vec<u8> = header.iter().flat_map(|w| w.to_be_bytes()).collect();
        out.extend_from_slice(&[0u8; 16]);
        out.extend_from_slice(&self.structure);
        out.extend_from_slice(&self.strings);
        out
    }
}
