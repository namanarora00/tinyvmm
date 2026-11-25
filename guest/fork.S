// Toy guest for milestone 2: prove that a running VM can be forked.
//
// No OS. The MMU is off, so every address is physical, and RAM starts at
// 0x8000_0000 (where the VMM loads this code). The addresses below have no
// RAM behind them, so touching them makes the vCPU exit to the VMM: that is
// how this guest "calls" its hypervisor.

.equ UART,   0x09000000     // write a byte: VMM prints it
.equ REPORT, 0x09001000     // write 8 bytes: VMM prints "counter = N"
.equ FORK,   0x09002000     // write anything: VMM forks this VM
.equ ROLE,   0x09003000     // read 8 bytes: 0 in the parent, i in child i
.equ PSCI_SYSTEM_OFF, 0x84000008

.global _start
_start:
    adr  x0, hello
    bl   puts

    adr  x5, counter        // a variable in guest RAM
    mov  x6, #42
    str  x6, [x5]           // counter = 42 before the fork

    ldr  x1, =FORK
    str  wzr, [x1]          // fork here

    ldr  x1, =ROLE
    ldr  x7, [x1]           // who am I?
    mov  x9, #1000
    cmp  x7, #0
    csel x9, x9, x7, eq     // step = 1000 in the parent, i in child i

    // Three times: counter += step, then report it. If each VM really has its
    // own copy of memory, the parent prints 1042, 2042, 3042 and child i
    // prints 42+i, 42+2i, 42+3i, with no cross-talk.
    mov  x8, #3
1:  ldr  x6, [x5]
    add  x6, x6, x9
    str  x6, [x5]
    ldr  x1, =REPORT
    str  x6, [x1]
    subs x8, x8, #1
    b.ne 1b

    ldr  x0, =PSCI_SYSTEM_OFF
    hvc  #0                 // KVM turns this into a SYSTEM_EVENT exit
    b    .

// puts(x0 = NUL-terminated string). Clobbers x0-x2. Leaf function: no stack.
puts:
    ldr  x1, =UART
2:  ldrb w2, [x0], #1
    cbz  w2, 3f
    strb w2, [x1]
    b    2b
3:  ret

.ltorg
.balign 8                   // MMU off = device memory: unaligned access faults
counter: .quad 0
hello:   .asciz "hello from the guest, forking now\n"
