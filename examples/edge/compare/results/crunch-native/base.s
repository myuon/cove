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
      89:      	je	0x432 <_f+0x432>
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
     12e:      	je	0x2e4 <_f+0x2e4>
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
     188:      	movabsq	$-0x8000000000000000, %rdx ## imm = 0x8000000000000000
     192:      	cmpq	%rdx, %rax
     195:      	jne	0x1da <_f+0x1da>
     19b:      	movabsq	$-0x1, %rdx
     1a5:      	cmpq	%rdx, %rcx
     1a8:      	jne	0x1da <_f+0x1da>
     1ae:      	movq	%r13, 0x68(%rbx)
     1b5:      	movl	$0x5, 0x78(%rbx)
     1bf:      	movl	$0x6, 0x7c(%rbx)
     1c9:      	movl	$0x1, %eax
     1ce:      	popq	%rbp
     1cf:      	popq	%rbp
     1d0:      	popq	%r15
     1d2:      	popq	%r14
     1d4:      	popq	%r13
     1d6:      	popq	%r12
     1d8:      	popq	%rbx
     1d9:      	retq
     1da:      	cqto
     1dc:      	idivq	%rcx
     1df:      	movq	%rdx, 0x38(%r14)
     1e6:      	movq	0x38(%r14), %rax
     1ed:      	movabsq	$0x0, %rcx
     1f7:      	cmpq	%rcx, %rax
     1fa:      	sete	%al
     1fd:      	movzbl	%al, %eax
     200:      	movq	%rax, 0x40(%r14)
     207:      	testq	%rax, %rax
     20a:      	je	0x237 <_f+0x237>
     210:      	addq	$0x2, %r13
     217:      	movabsq	$0x0, %rax
     221:      	movq	0x8(%rbx), %r14
     228:      	addq	%r12, %r14
     22b:      	movq	%rax, 0x20(%r14)
     232:      	jmp	0x306 <_f+0x306>
     237:      	addq	$0x2, %r13
     23e:      	movq	0x8(%rbx), %r14
     245:      	addq	%r12, %r14
     248:      	movq	0x30(%r14), %rax
     24f:      	movabsq	$0x2, %rcx
     259:      	addq	%rcx, %rax
     25c:      	jno	0x28e <_f+0x28e>
     262:      	movq	%r13, 0x68(%rbx)
     269:      	movl	$0x1, 0x78(%rbx)
     273:      	movl	$0xa, 0x7c(%rbx)
     27d:      	movl	$0x1, %eax
     282:      	popq	%rbp
     283:      	popq	%rbp
     284:      	popq	%r15
     286:      	popq	%r14
     288:      	popq	%r13
     28a:      	popq	%r12
     28c:      	popq	%rbx
     28d:      	retq
     28e:      	movq	%rax, 0x30(%r14)
     295:      	cmpq	0x70(%rbx), %r13
     29c:      	jb	0x2df <_f+0x2df>
     2a2:      	movq	%rbx, %rdi
     2a5:      	movl	$0x4, %esi
     2aa:      	movq	%r13, %rdx
     2ad:      	movabsq	$0x10b092cd0, %rax      ## imm = 0x10B092CD0
     2b7:      	callq	*%rax
     2b9:      	testb	%al, %al
     2bb:      	jne	0x2dc <_f+0x2dc>
     2c1:      	xorq	%rax, %rax
     2c4:      	movq	%rax, 0x68(%rbx)
     2cb:      	movl	$0x2, %eax
     2d0:      	popq	%rbp
     2d1:      	popq	%rbp
     2d2:      	popq	%r15
     2d4:      	popq	%r14
     2d6:      	popq	%r13
     2d8:      	popq	%r12
     2da:      	popq	%rbx
     2db:      	retq
     2dc:      	xorq	%r13, %r13
     2df:      	jmp	0xb1 <_f+0xb1>
     2e4:      	addq	$0x1, %r13
     2eb:      	movabsq	$0x1, %rax
     2f5:      	movq	0x8(%rbx), %r14
     2fc:      	addq	%r12, %r14
     2ff:      	movq	%rax, 0x20(%r14)
     306:      	addq	$0x1, %r13
     30d:      	movq	0x8(%rbx), %r14
     314:      	addq	%r12, %r14
     317:      	movq	0x20(%r14), %rax
     31e:      	testq	%rax, %rax
     321:      	je	0x385 <_f+0x385>
     327:      	addq	$0x1, %r13
     32e:      	movq	0x8(%rbx), %r14
     335:      	addq	%r12, %r14
     338:      	movq	0x10(%r14), %rax
     33f:      	movabsq	$0x1, %rcx
     349:      	addq	%rcx, %rax
     34c:      	jno	0x37e <_f+0x37e>
     352:      	movq	%r13, 0x68(%rbx)
     359:      	movl	$0x1, 0x78(%rbx)
     363:      	movl	$0xe, 0x7c(%rbx)
     36d:      	movl	$0x1, %eax
     372:      	popq	%rbp
     373:      	popq	%rbp
     374:      	popq	%r15
     376:      	popq	%r14
     378:      	popq	%r13
     37a:      	popq	%r12
     37c:      	popq	%rbx
     37d:      	retq
     37e:      	movq	%rax, 0x10(%r14)
     385:      	addq	$0x2, %r13
     38c:      	movq	0x8(%rbx), %r14
     393:      	addq	%r12, %r14
     396:      	movq	0x18(%r14), %rax
     39d:      	movabsq	$0x2, %rcx
     3a7:      	addq	%rcx, %rax
     3aa:      	jno	0x3dc <_f+0x3dc>
     3b0:      	movq	%r13, 0x68(%rbx)
     3b7:      	movl	$0x1, 0x78(%rbx)
     3c1:      	movl	$0xf, 0x7c(%rbx)
     3cb:      	movl	$0x1, %eax
     3d0:      	popq	%rbp
     3d1:      	popq	%rbp
     3d2:      	popq	%r15
     3d4:      	popq	%r14
     3d6:      	popq	%r13
     3d8:      	popq	%r12
     3da:      	popq	%rbx
     3db:      	retq
     3dc:      	movq	%rax, 0x18(%r14)
     3e3:      	cmpq	0x70(%rbx), %r13
     3ea:      	jb	0x42d <_f+0x42d>
     3f0:      	movq	%rbx, %rdi
     3f3:      	movl	$0x2, %esi
     3f8:      	movq	%r13, %rdx
     3fb:      	movabsq	$0x10b092cd0, %rax      ## imm = 0x10B092CD0
     405:      	callq	*%rax
     407:      	testb	%al, %al
     409:      	jne	0x42a <_f+0x42a>
     40f:      	xorq	%rax, %rax
     412:      	movq	%rax, 0x68(%rbx)
     419:      	movl	$0x2, %eax
     41e:      	popq	%rbp
     41f:      	popq	%rbp
     420:      	popq	%r15
     422:      	popq	%r14
     424:      	popq	%r13
     426:      	popq	%r12
     428:      	popq	%rbx
     429:      	retq
     42a:      	xorq	%r13, %r13
     42d:      	jmp	0x57 <_f+0x57>
     432:      	addq	$0x2, %r13
     439:      	movq	0x8(%rbx), %r14
     440:      	addq	%r12, %r14
     443:      	movq	0x10(%r14), %rax
     44a:      	pushq	%rax
     44b:      	popq	%rax
     44c:      	movq	%rax, 0x8(%r14)
     453:      	movq	%r13, 0x68(%rbx)
     45a:      	movq	0x8(%rbx), %rdx
     461:      	addq	%rbp, %rdx
     464:      	movq	0x8(%r14), %rax
     46b:      	movq	%rax, (%rdx)
     472:      	movl	$0x0, %eax
     477:      	popq	%rbp
     478:      	popq	%rbp
     479:      	popq	%r15
     47b:      	popq	%r14
     47d:      	popq	%r13
     47f:      	popq	%r12
     481:      	popq	%rbx
     482:      	retq
