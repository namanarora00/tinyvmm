//! Save and restore the in-kernel GICv3 (interrupt controller) state.
//!
//! The GIC is emulated inside KVM, so unlike our UART its state does not live
//! in our process memory, and fork() does not copy it. We read it out register
//! by register through device attributes, and write it into the child's fresh
//! GIC. Three parts (Documentation/virt/kvm/devices/arm-vgic-v3.rst):
//!   distributor     shared interrupts (SPIs, INTID >= 32): enabled, pending,
//!                   active, priority, trigger type, target CPU
//!   redistributor   per-CPU interrupts (SGIs/PPIs, INTID < 32), incl. the timer
//!   CPU interface   per-CPU system registers ICC_*: priority mask, enables,
//!                   which priorities are currently being serviced
//! All vCPUs must be out of KVM_RUN while we do this (KVM returns EBUSY otherwise).

use crate::kvm::{Device, VGIC_GRP_CPU_SYSREGS, VGIC_GRP_DIST_REGS, VGIC_GRP_REDIST_REGS};
use std::io;

#[derive(Clone)]
pub struct GicState {
    /// (attribute group, attribute, value), in restore order.
    pub regs: Vec<(u32, u64, u64)>,
}

/// The attr encoding of a CPU's affinity (bits 63..32): Aff3.Aff2.Aff1.Aff0.
fn mpidr_attr(mpidr: u64) -> u64 {
    let aff0 = mpidr & 0xff;
    let aff1 = (mpidr >> 8) & 0xff;
    let aff2 = (mpidr >> 16) & 0xff;
    let aff3 = (mpidr >> 32) & 0xff;
    ((aff3 << 24) | (aff2 << 16) | (aff1 << 8) | aff0) << 32
}

/// System register encoding used by the CPU_SYSREGS group.
const fn sysreg(op0: u64, op1: u64, crn: u64, crm: u64, op2: u64) -> u64 {
    (op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
}
const ICC_PMR_EL1: u64 = sysreg(3, 0, 4, 6, 0);
const ICC_BPR0_EL1: u64 = sysreg(3, 0, 12, 8, 3);
const ICC_BPR1_EL1: u64 = sysreg(3, 0, 12, 12, 3);
const ICC_CTLR_EL1: u64 = sysreg(3, 0, 12, 12, 4);
const ICC_SRE_EL1: u64 = sysreg(3, 0, 12, 12, 5);
const ICC_IGRPEN0_EL1: u64 = sysreg(3, 0, 12, 12, 6);
const ICC_IGRPEN1_EL1: u64 = sysreg(3, 0, 12, 12, 7);
const fn icc_ap0r(n: u64) -> u64 {
    sysreg(3, 0, 12, 8, 4 + n)
}
const fn icc_ap1r(n: u64) -> u64 {
    sysreg(3, 0, 12, 9, n)
}

/// Which registers to save, in the order they must be restored.
fn layout(gic: &Device, nr_irqs: u32, mpidr: u64) -> io::Result<Vec<(u32, u64)>> {
    let mut regs = Vec::new();
    let spis = (32..nr_irqs as u64).collect::<Vec<_>>();

    // Distributor. Bit-per-interrupt registers cover 32 interrupts per word,
    // priorities 4 per word (8 bits each), trigger config 16 per word.
    let d = |regs: &mut Vec<(u32, u64)>, off: u64| regs.push((VGIC_GRP_DIST_REGS, off));
    d(&mut regs, 0x0000); // GICD_CTLR
    for irq in spis.iter().step_by(32) {
        d(&mut regs, 0x0080 + irq / 8); // GICD_IGROUPR
        d(&mut regs, 0x0100 + irq / 8); // GICD_ISENABLER
        d(&mut regs, 0x0200 + irq / 8); // GICD_ISPENDR
        d(&mut regs, 0x0300 + irq / 8); // GICD_ISACTIVER
    }
    for irq in spis.iter().step_by(4) {
        d(&mut regs, 0x0400 + irq); // GICD_IPRIORITYR
    }
    for irq in spis.iter().step_by(16) {
        d(&mut regs, 0x0C00 + irq / 4); // GICD_ICFGR
    }
    for irq in &spis {
        d(&mut regs, 0x6000 + irq * 8); // GICD_IROUTER, low half
        d(&mut regs, 0x6000 + irq * 8 + 4); // high half
    }

    // Redistributor of CPU `mpidr`. Its second 64 KiB frame ("SGI frame")
    // holds the per-CPU interrupt registers.
    let cpu = mpidr_attr(mpidr);
    let sgi = 0x1_0000;
    let r = |regs: &mut Vec<(u32, u64)>, off: u64| regs.push((VGIC_GRP_REDIST_REGS, cpu | off));
    r(&mut regs, 0x0000); // GICR_CTLR
    r(&mut regs, sgi + 0x0080); // GICR_IGROUPR0
    r(&mut regs, sgi + 0x0100); // GICR_ISENABLER0
    r(&mut regs, sgi + 0x0200); // GICR_ISPENDR0
    r(&mut regs, sgi + 0x0300); // GICR_ISACTIVER0
    for i in 0..8 {
        r(&mut regs, sgi + 0x0400 + 4 * i); // GICR_IPRIORITYR0..7
    }
    r(&mut regs, sgi + 0x0C00); // GICR_ICFGR0
    r(&mut regs, sgi + 0x0C04); // GICR_ICFGR1

    // CPU interface. The number of active-priority registers depends on how
    // many priority bits this GIC implements (ICC_CTLR_EL1.PRIbits).
    let ctlr = read(gic, VGIC_GRP_CPU_SYSREGS, cpu | ICC_CTLR_EL1)?;
    let pri_bits = ((ctlr >> 8) & 0x7) + 1;
    let n_apr = match pri_bits {
        7 => 4,
        6 => 2,
        _ => 1,
    };
    for reg in [ICC_SRE_EL1, ICC_CTLR_EL1, ICC_IGRPEN0_EL1, ICC_IGRPEN1_EL1, ICC_PMR_EL1, ICC_BPR0_EL1, ICC_BPR1_EL1] {
        regs.push((VGIC_GRP_CPU_SYSREGS, cpu | reg));
    }
    for n in 0..n_apr {
        regs.push((VGIC_GRP_CPU_SYSREGS, cpu | icc_ap0r(n)));
        regs.push((VGIC_GRP_CPU_SYSREGS, cpu | icc_ap1r(n)));
    }
    Ok(regs)
}

fn read(gic: &Device, group: u32, attr: u64) -> io::Result<u64> {
    // Distributor/redistributor registers are 32-bit, ICC_* are 64-bit. A u64
    // buffer works for both on a little-endian machine.
    let mut v: u64 = 0;
    gic.get_attr(group, attr, &mut v as *mut u64 as u64)?;
    Ok(v)
}

pub fn save(gic: &Device, nr_irqs: u32, mpidr: u64) -> io::Result<GicState> {
    let regs = layout(gic, nr_irqs, mpidr)?
        .into_iter()
        .map(|(group, attr)| Ok((group, attr, read(gic, group, attr)?)))
        .collect::<io::Result<_>>()?;
    Ok(GicState { regs })
}

/// Load the saved state into a freshly initialised GIC. Returns the
/// registers KVM refused.
pub fn restore(gic: &Device, state: &GicState) -> Vec<(u32, u64, io::Error)> {
    state
        .regs
        .iter()
        .filter_map(|&(group, attr, v)| gic.set_attr(group, attr, &v as *const u64 as u64).err().map(|e| (group, attr, e)))
        .collect()
}
