# crunch's primesUpTo, with isOddPrime inlined, as Go 1.23.2 compiles
# compare/go/handlers.go for darwin/amd64 (`go tool objdump -s main.crunch
# edge-go`), with its registers renamed and the panic path cut to a return.
# Every variable is in a register, the division is `idivq`, there is no
# overflow check and no poll on either backedge (Go preempts asynchronously).
#
#   long V_go(long n, long *unused)    the count of primes up to n
#   long V_go_div32(long n, long *unused)
#                                      the same with a 32-bit `idivl`: not
#                                      Go's code, a probe of how much of the
#                                      loop is the 64-bit division
.text
.globl _V_go
.p2align 6
_V_go:
	pushq %rbx
	movq %rdi, %r11           # n
	movl $1, %edi             # count
	movl $3, %ecx             # candidate
	jmp 2f
1:	addq $2, %rcx             # candidate += 2
2:	cmpq %r11, %rcx
	jg 9f
	movl $3, %esi             # divisor
3:	movq %rsi, %rbx
	imulq %rbx, %rbx
	cmpq %rbx, %rcx
	jl 5f                     # candidate < divisor*divisor: prime
	testq %rsi, %rsi
	je 8f                     # runtime.panicdivide
	movq %rcx, %rax
	cmpq $-1, %rsi
	jne 4f
	negq %rax
	xorl %edx, %edx
	jmp 6f
4:	cqto
	idivq %rsi
6:	testq %rdx, %rdx
	je 1b                     # divisible: not prime
	addq $2, %rsi
	jmp 3b
5:	incq %rdi
	jmp 1b
8:	movq $-1, %rax
	popq %rbx
	retq
9:	movq %rdi, %rax
	popq %rbx
	retq

.globl _V_go_div32
.p2align 6
_V_go_div32:
	pushq %rbx
	movq %rdi, %r11
	movl $1, %edi
	movl $3, %ecx
	jmp 2f
1:	addq $2, %rcx
2:	cmpq %r11, %rcx
	jg 9f
	movl $3, %esi
3:	movq %rsi, %rbx
	imulq %rbx, %rbx
	cmpq %rbx, %rcx
	jl 5f
	testq %rsi, %rsi
	je 8f
	movl %ecx, %eax
	cltd
	idivl %esi
6:	testl %edx, %edx
	je 1b
	addq $2, %rsi
	jmp 3b
5:	incq %rdi
	jmp 1b
8:	movq $-1, %rax
	popq %rbx
	retq
9:	movq %rdi, %rax
	popq %rbx
	retq
