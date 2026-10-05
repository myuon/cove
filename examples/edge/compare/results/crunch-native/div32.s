       0:      	pushq	%rbx
       1:      	pushq	%r12
       3:      	pushq	%r13
       5:      	pushq	%r14
       7:      	pushq	%r15
       9:      	pushq	%rbp
       a:      	pushq	%rbp
       b:      	movq	%rdi, %rbx
       e:      	movq	%rsi, %r12
      11:      	shlq	$0x3, %r12
      15:      	movl	%ecx, %ecx
      17:      	movq	%rdx, %rbp
      1a:      	addq	%rcx, %rbp
      1d:      	shlq	$0x3, %rbp
      21:      	xorq	%r13, %r13
      24:      	addq	$0x2, %r13
      2b:      	movabsq	$0x1, %rax
      35:      	movq	0x8(%rbx), %r14
      3c:      	addq	%r12, %r14
      3f:      	movq	%rax, 0x10(%r14)
      46:      	movabsq	$0x3, %rax
      50:      	movq	%rax, 0x18(%r14)
      57:      	addq	$0x1, %r13
      5e:      	movq	0x8(%rbx), %r14
      65:      	addq	%r12, %r14
      68:      	movq	0x18(%r14), %rax
      6f:      	movq	(%r14), %rcx
      76:      	cmpq	%rcx, %rax
      79:      	setle	%al
      7c:      	movzbl	%al, %eax
      7f:      	movq	%rax, 0x20(%r14)
      86:      	testq	%rax, %rax
      89:      	je	0x44c <_f+0x44c>
      8f:      	addq	$0x1, %r13
      96:      	movabsq	$0x3, %rax
      a0:      	movq	0x8(%rbx), %r14
      a7:      	addq	%r12, %r14
      aa:      	movq	%rax, 0x30(%r14)
      b1:      	addq	$0x2, %r13
      b8:      	movq	0x8(%rbx), %r14
      bf:      	addq	%r12, %r14
      c2:      	movq	0x30(%r14), %rax
      c9:      	movq	0x30(%r14), %rcx
      d0:      	imulq	%rcx, %rax
      d4:      	jno	0x106 <_f+0x106>
      da:      	movq	%r13, 0x68(%rbx)
      e1:      	movl	$0x3, 0x78(%rbx)
      eb:      	movl	$0x4, 0x7c(%rbx)
      f5:      	movl	$0x1, %eax
      fa:      	popq	%rbp
      fb:      	popq	%rbp
      fc:      	popq	%r15
      fe:      	popq	%r14
     100:      	popq	%r13
     102:      	popq	%r12
     104:      	popq	%rbx
     105:      	retq
     106:      	movq	%rax, 0x38(%r14)
     10d:      	movq	0x38(%r14), %rax
     114:      	movq	0x18(%r14), %rcx
     11b:      	cmpq	%rcx, %rax
     11e:      	setle	%al
     121:      	movzbl	%al, %eax
     124:      	movq	%rax, 0x40(%r14)
     12b:      	testq	%rax, %rax
     12e:      	je	0x2fe <_f+0x2fe>
     134:      	addq	$0x2, %r13
     13b:      	movq	0x8(%rbx), %r14
     142:      	addq	%r12, %r14
     145:      	movq	0x18(%r14), %rax
     14c:      	movq	0x30(%r14), %rcx
     153:      	testq	%rcx, %rcx
     156:      	jne	0x188 <_f+0x188>
     15c:      	movq	%r13, 0x68(%rbx)
     163:      	movl	$0x8, 0x78(%rbx)
     16d:      	movl	$0x6, 0x7c(%rbx)
     177:      	movl	$0x1, %eax
     17c:      	popq	%rbp
     17d:      	popq	%rbp
     17e:      	popq	%r15
     180:      	popq	%r14
     182:      	popq	%r13
     184:      	popq	%r12
     186:      	popq	%rbx
     187:      	retq
     188:      	movq	%rax, %rdx
     18b:      	orq	%rcx, %rdx
     18e:      	shrq	$0x20, %rdx
     192:      	jne	0x1a2 <_f+0x1a2>
     198:      	xorq	%rdx, %rdx
     19b:      	divl	%ecx
     19d:      	jmp	0x1f9 <_f+0x1f9>
     1a2:      	movabsq	$-0x8000000000000000, %rdx ## imm = 0x8000000000000000
     1ac:      	cmpq	%rdx, %rax
     1af:      	jne	0x1f4 <_f+0x1f4>
     1b5:      	movabsq	$-0x1, %rdx
     1bf:      	cmpq	%rdx, %rcx
     1c2:      	jne	0x1f4 <_f+0x1f4>
     1c8:      	movq	%r13, 0x68(%rbx)
     1cf:      	movl	$0x5, 0x78(%rbx)
     1d9:      	movl	$0x6, 0x7c(%rbx)
     1e3:      	movl	$0x1, %eax
     1e8:      	popq	%rbp
     1e9:      	popq	%rbp
     1ea:      	popq	%r15
     1ec:      	popq	%r14
     1ee:      	popq	%r13
     1f0:      	popq	%r12
     1f2:      	popq	%rbx
     1f3:      	retq
     1f4:      	cqto
     1f6:      	idivq	%rcx
     1f9:      	movq	%rdx, 0x38(%r14)
     200:      	movq	0x38(%r14), %rax
     207:      	movabsq	$0x0, %rcx
     211:      	cmpq	%rcx, %rax
     214:      	sete	%al
     217:      	movzbl	%al, %eax
     21a:      	movq	%rax, 0x40(%r14)
     221:      	testq	%rax, %rax
     224:      	je	0x251 <_f+0x251>
     22a:      	addq	$0x2, %r13
     231:      	movabsq	$0x0, %rax
     23b:      	movq	0x8(%rbx), %r14
     242:      	addq	%r12, %r14
     245:      	movq	%rax, 0x20(%r14)
     24c:      	jmp	0x320 <_f+0x320>
     251:      	addq	$0x2, %r13
     258:      	movq	0x8(%rbx), %r14
     25f:      	addq	%r12, %r14
     262:      	movq	0x30(%r14), %rax
     269:      	movabsq	$0x2, %rcx
     273:      	addq	%rcx, %rax
     276:      	jno	0x2a8 <_f+0x2a8>
     27c:      	movq	%r13, 0x68(%rbx)
     283:      	movl	$0x1, 0x78(%rbx)
     28d:      	movl	$0xa, 0x7c(%rbx)
     297:      	movl	$0x1, %eax
     29c:      	popq	%rbp
     29d:      	popq	%rbp
     29e:      	popq	%r15
     2a0:      	popq	%r14
     2a2:      	popq	%r13
     2a4:      	popq	%r12
     2a6:      	popq	%rbx
     2a7:      	retq
     2a8:      	movq	%rax, 0x30(%r14)
     2af:      	cmpq	0x70(%rbx), %r13
     2b6:      	jb	0x2f9 <_f+0x2f9>
     2bc:      	movq	%rbx, %rdi
     2bf:      	movl	$0x4, %esi
     2c4:      	movq	%r13, %rdx
     2c7:      	movabsq	$0x10fc9ddf0, %rax      ## imm = 0x10FC9DDF0
     2d1:      	callq	*%rax
     2d3:      	testb	%al, %al
     2d5:      	jne	0x2f6 <_f+0x2f6>
     2db:      	xorq	%rax, %rax
     2de:      	movq	%rax, 0x68(%rbx)
     2e5:      	movl	$0x2, %eax
     2ea:      	popq	%rbp
     2eb:      	popq	%rbp
     2ec:      	popq	%r15
     2ee:      	popq	%r14
     2f0:      	popq	%r13
     2f2:      	popq	%r12
     2f4:      	popq	%rbx
     2f5:      	retq
     2f6:      	xorq	%r13, %r13
     2f9:      	jmp	0xb1 <_f+0xb1>
     2fe:      	addq	$0x1, %r13
     305:      	movabsq	$0x1, %rax
     30f:      	movq	0x8(%rbx), %r14
     316:      	addq	%r12, %r14
     319:      	movq	%rax, 0x20(%r14)
     320:      	addq	$0x1, %r13
     327:      	movq	0x8(%rbx), %r14
     32e:      	addq	%r12, %r14
     331:      	movq	0x20(%r14), %rax
     338:      	testq	%rax, %rax
     33b:      	je	0x39f <_f+0x39f>
     341:      	addq	$0x1, %r13
     348:      	movq	0x8(%rbx), %r14
     34f:      	addq	%r12, %r14
     352:      	movq	0x10(%r14), %rax
     359:      	movabsq	$0x1, %rcx
     363:      	addq	%rcx, %rax
     366:      	jno	0x398 <_f+0x398>
     36c:      	movq	%r13, 0x68(%rbx)
     373:      	movl	$0x1, 0x78(%rbx)
     37d:      	movl	$0xe, 0x7c(%rbx)
     387:      	movl	$0x1, %eax
     38c:      	popq	%rbp
     38d:      	popq	%rbp
     38e:      	popq	%r15
     390:      	popq	%r14
     392:      	popq	%r13
     394:      	popq	%r12
     396:      	popq	%rbx
     397:      	retq
     398:      	movq	%rax, 0x10(%r14)
     39f:      	addq	$0x2, %r13
     3a6:      	movq	0x8(%rbx), %r14
     3ad:      	addq	%r12, %r14
     3b0:      	movq	0x18(%r14), %rax
     3b7:      	movabsq	$0x2, %rcx
     3c1:      	addq	%rcx, %rax
     3c4:      	jno	0x3f6 <_f+0x3f6>
     3ca:      	movq	%r13, 0x68(%rbx)
     3d1:      	movl	$0x1, 0x78(%rbx)
     3db:      	movl	$0xf, 0x7c(%rbx)
     3e5:      	movl	$0x1, %eax
     3ea:      	popq	%rbp
     3eb:      	popq	%rbp
     3ec:      	popq	%r15
     3ee:      	popq	%r14
     3f0:      	popq	%r13
     3f2:      	popq	%r12
     3f4:      	popq	%rbx
     3f5:      	retq
     3f6:      	movq	%rax, 0x18(%r14)
     3fd:      	cmpq	0x70(%rbx), %r13
     404:      	jb	0x447 <_f+0x447>
     40a:      	movq	%rbx, %rdi
     40d:      	movl	$0x2, %esi
     412:      	movq	%r13, %rdx
     415:      	movabsq	$0x10fc9ddf0, %rax      ## imm = 0x10FC9DDF0
     41f:      	callq	*%rax
     421:      	testb	%al, %al
     423:      	jne	0x444 <_f+0x444>
     429:      	xorq	%rax, %rax
     42c:      	movq	%rax, 0x68(%rbx)
     433:      	movl	$0x2, %eax
     438:      	popq	%rbp
     439:      	popq	%rbp
     43a:      	popq	%r15
     43c:      	popq	%r14
     43e:      	popq	%r13
     440:      	popq	%r12
     442:      	popq	%rbx
     443:      	retq
     444:      	xorq	%r13, %r13
     447:      	jmp	0x57 <_f+0x57>
     44c:      	addq	$0x2, %r13
     453:      	movq	0x8(%rbx), %r14
     45a:      	addq	%r12, %r14
     45d:      	movq	0x10(%r14), %rax
     464:      	pushq	%rax
     465:      	popq	%rax
     466:      	movq	%rax, 0x8(%r14)
     46d:      	movq	%r13, 0x68(%rbx)
     474:      	movq	0x8(%rbx), %rdx
     47b:      	addq	%rbp, %rdx
     47e:      	movq	0x8(%r14), %rax
     485:      	movq	%rax, (%rdx)
     48c:      	movl	$0x0, %eax
     491:      	popq	%rbp
     492:      	popq	%rbp
     493:      	popq	%r15
     495:      	popq	%r14
     497:      	popq	%r13
     499:      	popq	%r12
     49b:      	popq	%rbx
     49c:      	retq
