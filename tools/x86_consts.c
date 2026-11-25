/* Print the x86 KVM ioctl numbers from the real kernel header, to check the
 * constants hard-coded in src/x86.rs. Build and run on an x86_64 host:
 *   gcc -o /tmp/x86_consts tools/x86_consts.c && /tmp/x86_consts */
#include <linux/kvm.h>
#include <stdio.h>
#define P(x) printf("%-28s 0x%08lX\n", #x, (unsigned long)(x))
int main(void) {
    P(KVM_GET_MSR_INDEX_LIST); P(KVM_GET_SUPPORTED_CPUID); P(KVM_SET_TSS_ADDR); P(KVM_SET_IDENTITY_MAP_ADDR);
    P(KVM_CREATE_IRQCHIP); P(KVM_GET_IRQCHIP); P(KVM_SET_IRQCHIP); P(KVM_CREATE_PIT2); P(KVM_GET_PIT2); P(KVM_SET_PIT2);
    P(KVM_SET_CLOCK); P(KVM_GET_CLOCK); P(KVM_GET_REGS); P(KVM_SET_REGS); P(KVM_GET_SREGS); P(KVM_SET_SREGS);
    P(KVM_GET_MSRS); P(KVM_SET_MSRS); P(KVM_GET_LAPIC); P(KVM_SET_LAPIC); P(KVM_SET_CPUID2); P(KVM_GET_MP_STATE);
    P(KVM_SET_MP_STATE); P(KVM_GET_VCPU_EVENTS); P(KVM_SET_VCPU_EVENTS); P(KVM_GET_DEBUGREGS); P(KVM_SET_DEBUGREGS);
    P(KVM_GET_XSAVE); P(KVM_SET_XSAVE); P(KVM_GET_XCRS); P(KVM_SET_XCRS); P(KVM_IRQ_LINE);
    return 0;
}
