//! Raw KVM for arm64: ioctl numbers, struct layouts and thin wrappers.
//!
//! No KVM crates on purpose: everything here is transcribed from
//! include/uapi/linux/kvm.h and arch/arm64/include/uapi/asm/kvm.h, so you can
//! see that the whole hypervisor interface is "ioctls on file descriptors".
//!
//! Three kinds of fd:
//!   /dev/kvm  -> system: version checks, create VMs. Usable from any process.
//!   VM fd     -> memory map, devices, create vCPUs. Tied to the creating
//!                process's address space: other processes get EIO.
//!   vCPU fd   -> registers, KVM_RUN. Same restriction as the VM fd.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

// ioctl encoding: dir << 30 | size << 16 | type << 8 | nr, with type = 0xAE.
// _IO = no data, _IOW = we pass data in, _IOR = kernel fills data, _IOWR = both.
const KVM_GET_API_VERSION: u64 = 0xAE00;
const KVM_CREATE_VM: u64 = 0xAE01;
const KVM_GET_VCPU_MMAP_SIZE: u64 = 0xAE04;
const KVM_CREATE_VCPU: u64 = 0xAE41;
const KVM_SET_USER_MEMORY_REGION: u64 = 0x4020_AE46; // _IOW, 32-byte struct
const KVM_RUN: u64 = 0xAE80;
const KVM_GET_ONE_REG: u64 = 0x4010_AEAB; // _IOW: we pass {id, pointer}
const KVM_SET_ONE_REG: u64 = 0x4010_AEAC;
const KVM_ARM_VCPU_INIT: u64 = 0x4020_AEAE;
const KVM_ARM_PREFERRED_TARGET: u64 = 0x8020_AEAF; // _IOR
const KVM_GET_REG_LIST: u64 = 0xC008_AEB0; // _IOWR, 8-byte header + array

const KVM_IRQ_LINE: u64 = 0x4008_AE61; // _IOW, struct kvm_irq_level (8 bytes)
const KVM_CREATE_DEVICE: u64 = 0xC00C_AEE0; // _IOWR, struct kvm_create_device (12 bytes)
const KVM_SET_DEVICE_ATTR: u64 = 0x4018_AEE1; // _IOW, struct kvm_device_attr (24 bytes)
const KVM_GET_DEVICE_ATTR: u64 = 0x4018_AEE2;

/// In-kernel device types (enum kvm_device_type).
pub const KVM_DEV_TYPE_ARM_VGIC_V3: u32 = 7;

// GICv3 device attribute groups and attributes (asm/kvm.h).
pub const VGIC_GRP_ADDR: u32 = 0;
pub const VGIC_GRP_DIST_REGS: u32 = 1;
pub const VGIC_GRP_REDIST_REGS: u32 = 5;
pub const VGIC_GRP_CPU_SYSREGS: u32 = 6;
pub const VGIC_GRP_NR_IRQS: u32 = 3;
pub const VGIC_GRP_CTRL: u32 = 4;
pub const VGIC_V3_ADDR_TYPE_DIST: u64 = 2;
pub const VGIC_V3_ADDR_TYPE_REDIST: u64 = 3;
pub const VGIC_CTRL_INIT: u64 = 0;

const KVM_EXIT_IO: u32 = 2; // x86 port I/O (in/out instructions)
const KVM_EXIT_HLT: u32 = 5;
const KVM_EXIT_MMIO: u32 = 6;
const KVM_EXIT_SHUTDOWN: u32 = 8; // x86 triple fault
const KVM_EXIT_SYSTEM_EVENT: u32 = 24;

/// vCPU feature bit: let the guest use PSCI 0.2 (power off, CPU on/off).
pub const KVM_ARM_VCPU_PSCI_0_2: u32 = 2;

/// Register id of the program counter.
/// KVM_REG_ARM64 | KVM_REG_SIZE_U64 | KVM_REG_ARM_CORE | (offset in kvm_regs / 4).
/// kvm_regs begins with x0..x30 (248 bytes), sp (8), then pc: offset 256 -> 0x40.
pub const REG_PC: u64 = 0x6030_0000_0010_0040;

fn ioctl(fd: RawFd, req: u64, arg: u64) -> io::Result<i32> {
    let ret = unsafe { libc::ioctl(fd, req as _, arg) };
    if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(ret) }
}

pub struct Kvm {
    fd: File,
    run_size: usize,
}

impl Kvm {
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn open() -> io::Result<Self> {
        let fd = File::options().read(true).write(true).open("/dev/kvm")?;
        let version = ioctl(fd.as_raw_fd(), KVM_GET_API_VERSION, 0)?;
        assert_eq!(version, 12, "KVM API version has been 12 since 2007");
        let run_size = ioctl(fd.as_raw_fd(), KVM_GET_VCPU_MMAP_SIZE, 0)? as usize;
        Ok(Kvm { fd, run_size })
    }

    pub fn create_vm(&self) -> io::Result<Vm> {
        let fd = ioctl(self.fd.as_raw_fd(), KVM_CREATE_VM, 0)?;
        Ok(Vm { fd: unsafe { OwnedFd::from_raw_fd(fd) }, run_size: self.run_size })
    }
}

#[repr(C)]
struct UserspaceMemoryRegion {
    slot: u32,
    flags: u32,
    guest_phys_addr: u64,
    memory_size: u64,
    userspace_addr: u64,
}

/// struct kvm_vcpu_init: which CPU model, plus feature bits.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct VcpuInit {
    pub target: u32,
    pub features: [u32; 7],
}

pub struct Vm {
    fd: OwnedFd,
    run_size: usize,
}

impl Vm {
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Drive interrupt line `irq` (x86: a GSI, e.g. 4 for COM1).
    pub fn set_irq_line(&self, irq: u32, level: bool) -> io::Result<()> {
        let irq_level: [u32; 2] = [irq, level as u32];
        ioctl(self.fd.as_raw_fd(), KVM_IRQ_LINE, irq_level.as_ptr() as u64)?;
        Ok(())
    }

    /// Map `size` bytes of our address space at `host` to guest physical `guest_addr`.
    pub fn set_memory(&self, slot: u32, guest_addr: u64, host: *mut u8, size: usize) -> io::Result<()> {
        let region = UserspaceMemoryRegion {
            slot,
            flags: 0,
            guest_phys_addr: guest_addr,
            memory_size: size as u64,
            userspace_addr: host as u64,
        };
        ioctl(self.fd.as_raw_fd(), KVM_SET_USER_MEMORY_REGION, &region as *const _ as u64)?;
        Ok(())
    }

    /// The CPU model KVM can emulate on this host.
    pub fn preferred_target(&self) -> io::Result<VcpuInit> {
        let mut init = VcpuInit::default();
        ioctl(self.fd.as_raw_fd(), KVM_ARM_PREFERRED_TARGET, &mut init as *mut _ as u64)?;
        Ok(init)
    }

    /// Create an in-kernel device (here: the GIC). Returns its own fd.
    pub fn create_device(&self, kind: u32) -> io::Result<Device> {
        #[repr(C)]
        struct CreateDevice {
            kind: u32,
            fd: u32,
            flags: u32,
        }
        let mut cd = CreateDevice { kind, fd: 0, flags: 0 };
        ioctl(self.fd.as_raw_fd(), KVM_CREATE_DEVICE, &mut cd as *mut _ as u64)?;
        Ok(Device { fd: unsafe { OwnedFd::from_raw_fd(cd.fd as RawFd) } })
    }

    /// Drive a shared peripheral interrupt (SPI) line on the in-kernel GIC.
    /// `intid` is the GIC interrupt id, 32..1019. Safe to call from any thread
    /// while a vCPU runs: KVM kicks the vCPU so it sees the interrupt.
    pub fn set_spi(&self, intid: u32, level: bool) -> io::Result<()> {
        const KVM_ARM_IRQ_TYPE_SPI: u32 = 1;
        let irq_level: [u32; 2] = [(KVM_ARM_IRQ_TYPE_SPI << 24) | intid, level as u32];
        ioctl(self.fd.as_raw_fd(), KVM_IRQ_LINE, irq_level.as_ptr() as u64)?;
        Ok(())
    }

    pub fn create_vcpu(&self, id: u64) -> io::Result<Vcpu> {
        let fd = ioctl(self.fd.as_raw_fd(), KVM_CREATE_VCPU, id)?;
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // kvm_run: a page shared with the kernel that explains each exit.
        let run = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                self.run_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if run == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Vcpu { fd, run: run as *mut u8, run_size: self.run_size })
    }
}

/// An in-kernel device, configured through attributes.
pub struct Device {
    fd: OwnedFd,
}

#[repr(C)]
struct DeviceAttr {
    flags: u32,
    group: u32,
    attr: u64,
    addr: u64,
}

impl Device {
    /// `addr` points at the attribute's value in our memory (or is 0).
    pub fn set_attr(&self, group: u32, attr: u64, addr: u64) -> io::Result<()> {
        let a = DeviceAttr { flags: 0, group, attr, addr };
        ioctl(self.fd.as_raw_fd(), KVM_SET_DEVICE_ATTR, &a as *const _ as u64)?;
        Ok(())
    }

    /// Read an attribute into the buffer at `addr`.
    pub fn get_attr(&self, group: u32, attr: u64, addr: u64) -> io::Result<()> {
        let a = DeviceAttr { flags: 0, group, attr, addr };
        ioctl(self.fd.as_raw_fd(), KVM_GET_DEVICE_ATTR, &a as *const _ as u64)?;
        Ok(())
    }
}

/// Why KVM_RUN returned. Data is copied out so the vCPU isn't borrowed.
#[derive(Debug)]
#[allow(dead_code)] // fields are read through Debug when an exit is unhandled
pub enum Exit {
    MmioWrite { addr: u64, data: u64, len: usize },
    MmioRead { addr: u64, len: usize },
    SystemEvent(u32),
    /// x86 port I/O. `data` holds count * size bytes for an OUT.
    IoOut { port: u16, size: usize, data: Vec<u8> },
    IoIn { port: u16, size: usize, count: usize },
    Hlt,
    Shutdown,
    /// A signal interrupted KVM_RUN (used later to pause a vCPU from outside).
    Interrupted,
    Other(u32),
}

/// A saved register: id plus up to 16 bytes (the widest non-SVE register).
#[derive(Clone)]
pub struct CpuState {
    pub regs: Vec<(u64, [u8; 16])>,
}

impl CpuState {
    pub fn get_u64(&self, id: u64) -> u64 {
        let (_, v) = self.regs.iter().find(|(r, _)| *r == id).expect("register not saved");
        u64::from_le_bytes(v[..8].try_into().unwrap())
    }
    pub fn set_u64(&mut self, id: u64, value: u64) {
        let (_, v) = self.regs.iter_mut().find(|(r, _)| *r == id).expect("register not saved");
        v[..8].copy_from_slice(&value.to_le_bytes());
    }
}

/// Size in bytes encoded in a register id (bits 52..55 hold log2 of the size).
fn reg_size(id: u64) -> usize {
    1 << ((id >> 52) & 0xf)
}

#[repr(C)]
struct OneReg {
    id: u64,
    addr: u64,
}

pub struct Vcpu {
    fd: OwnedFd,
    run: *mut u8,
    run_size: usize,
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.run as *mut _, self.run_size) };
    }
}

impl Vcpu {
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// The shared kvm_run page. A signal handler writes `immediate_exit`
    /// (byte 1) through this pointer to kick the vCPU out of KVM_RUN.
    pub fn run_page(&self) -> *mut u8 {
        self.run
    }

    /// While set, KVM_RUN finishes any half-done MMIO access, then returns
    /// EINTR without running another guest instruction.
    pub fn set_immediate_exit(&self, on: bool) {
        unsafe { self.run.add(1).write_volatile(on as u8) };
    }

    pub fn init(&self, init: &VcpuInit) -> io::Result<()> {
        ioctl(self.fd.as_raw_fd(), KVM_ARM_VCPU_INIT, init as *const _ as u64)?;
        Ok(())
    }

    pub fn set_reg(&self, id: u64, bytes: &[u8; 16]) -> io::Result<()> {
        assert!(reg_size(id) <= 16);
        let reg = OneReg { id, addr: bytes.as_ptr() as u64 };
        ioctl(self.fd.as_raw_fd(), KVM_SET_ONE_REG, &reg as *const _ as u64)?;
        Ok(())
    }

    pub fn get_reg(&self, id: u64) -> io::Result<[u8; 16]> {
        assert!(reg_size(id) <= 16, "register {id:#x} wider than 16 bytes (SVE?)");
        let mut bytes = [0u8; 16];
        let reg = OneReg { id, addr: bytes.as_mut_ptr() as u64 };
        ioctl(self.fd.as_raw_fd(), KVM_GET_ONE_REG, &reg as *const _ as u64)?;
        Ok(bytes)
    }

    pub fn set_u64(&self, id: u64, value: u64) -> io::Result<()> {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&value.to_le_bytes());
        self.set_reg(id, &bytes)
    }

    /// Every register KVM will save/restore for this vCPU: core regs, system
    /// regs, FP/SIMD, timer, firmware pseudo-registers.
    pub fn reg_list(&self) -> io::Result<Vec<u64>> {
        // First call with n = 0 fails with E2BIG and tells us the real count.
        let mut header = [0u64; 1];
        let _ = ioctl(self.fd.as_raw_fd(), KVM_GET_REG_LIST, header.as_mut_ptr() as u64);
        let n = header[0] as usize;
        let mut buf = vec![0u64; n + 1];
        buf[0] = n as u64;
        ioctl(self.fd.as_raw_fd(), KVM_GET_REG_LIST, buf.as_mut_ptr() as u64)?;
        Ok(buf[1..].to_vec())
    }

    pub fn save(&self, ids: &[u64]) -> io::Result<CpuState> {
        let regs = ids.iter().map(|&id| Ok((id, self.get_reg(id)?))).collect::<io::Result<_>>()?;
        Ok(CpuState { regs })
    }

    /// Load every saved register. Returns the ones KVM refused, so the caller
    /// can decide whether they matter.
    pub fn restore(&self, state: &CpuState) -> Vec<(u64, io::Error)> {
        state
            .regs
            .iter()
            .filter_map(|(id, v)| self.set_reg(*id, v).err().map(|e| (*id, e)))
            .collect()
    }

    pub fn run(&self) -> io::Result<Exit> {
        if let Err(e) = ioctl(self.fd.as_raw_fd(), KVM_RUN, 0) {
            if e.raw_os_error() == Some(libc::EINTR) {
                return Ok(Exit::Interrupted);
            }
            return Err(e);
        }
        // struct kvm_run: exit_reason at offset 8, the per-exit union at 32.
        //   mmio = { phys_addr u64 @32, data [u8; 8] @40, len u32 @48, is_write u8 @52 }
        //   system_event = { type u32 @32, ... }
        let p = self.run;
        let u32_at = |off| unsafe { (p.add(off) as *const u32).read_unaligned() };
        let u64_at = |off| unsafe { (p.add(off) as *const u64).read_unaligned() };
        Ok(match u32_at(8) {
            KVM_EXIT_MMIO => {
                let addr = u64_at(32);
                let len = u32_at(48) as usize;
                if unsafe { *p.add(52) } != 0 {
                    let mut data = [0u8; 8];
                    data[..len].copy_from_slice(unsafe { std::slice::from_raw_parts(p.add(40), len) });
                    Exit::MmioWrite { addr, data: u64::from_le_bytes(data), len }
                } else {
                    Exit::MmioRead { addr, len }
                }
            }
            KVM_EXIT_SYSTEM_EVENT => Exit::SystemEvent(u32_at(32)),
            KVM_EXIT_IO => {
                // io = { direction u8 @32 (0 in, 1 out), size u8 @33, port u16 @34,
                //        count u32 @36, data_offset u64 @40 }; data lives in this page.
                let size = unsafe { *p.add(33) } as usize;
                let port = unsafe { (p.add(34) as *const u16).read_unaligned() };
                let count = u32_at(36) as usize;
                if unsafe { *p.add(32) } == 1 {
                    let off = u64_at(40) as usize;
                    let data = unsafe { std::slice::from_raw_parts(p.add(off), size * count) }.to_vec();
                    Exit::IoOut { port, size, data }
                } else {
                    Exit::IoIn { port, size, count }
                }
            }
            KVM_EXIT_HLT => Exit::Hlt,
            KVM_EXIT_SHUTDOWN => Exit::Shutdown,
            other => Exit::Other(other),
        })
    }

    /// Answer an x86 IN: KVM copies these bytes into the guest's register on
    /// the next KVM_RUN. For a string IN (count > 1) `data` covers every element.
    pub fn set_io_in(&self, data: &[u8]) {
        let p = self.run;
        let off = unsafe { (p.add(40) as *const u64).read_unaligned() } as usize;
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), p.add(off), data.len()) };
    }

    /// Answer an MMIO read: KVM puts these bytes in the guest's register on
    /// the next KVM_RUN.
    pub fn set_mmio_read(&self, value: u64, len: usize) {
        let bytes = value.to_le_bytes();
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.run.add(40), len) };
    }
}
