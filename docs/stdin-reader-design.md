# Native stdin reader — design notes (IMPLEMENTED)

## Goal
`--stdin` flag produces a native binary that reads whitespace-separated tokens
from fd 0 (stdin) instead of argc/argv. Enables piping:
```
echo "Acme 5000 BadCo 0" | ./validated
cat data.txt | ./payroll_report
```

## Strategy
Prepend a "stdin prologue" before the rule's code. After it runs, the stack
looks exactly like the kernel's _start layout (argc at [rsp], argv pointers
after), so the rule's standard prologue works unchanged.

## Layout
```
_start:
  mov rbx, rsp                  ; save original rsp (argc/argv target)
  sub rsp, 131072               ; 128K: 64K read buffer + 64K ptr array
  mov rsi, rsp                  ; buffer = rsp
  sys_read(0, rsi, 65536)       ; read stdin
  NUL-terminate: [rsi + rax] = 0
  lea r8, [rsi + 65536]         ; ptr array = buffer + 64K
  tokenize: walk buffer, NUL on whitespace, store ptrs at r8[r9++]
  copy ptrs to argc/argv layout at rbx:
    [rbx]      = r9 + 1 (argc)
    [rbx + 8]  = 0 (dummy argv[0])
    [rbx + 16] = token_ptrs[0]
    ...
  mov rsp, rbx                  ; restore rsp → rule prologue sees layout
```

## Known bugs from first attempt
1. **REX prefix for `mov [r8 + r9*8], rcx`** — needs REX.WXB (0x4B), NOT
   REX.WRX (0x4E). When both base (r8) and index (r9) are extended registers,
   REX.B covers the base and REX.X covers the index. REX.R is for the reg
   field (source/dest register), which is rcx (no extension needed).

2. **Tokenizer loop needs careful rel8 jump patching** — many forward/backward
   short jumps with multiple patch sites. Easy to get wrong with manual byte
   emission. Consider extracting a helper `emit_cmp_jcc_rel8_pair(code, cmp_bytes, patch_list)`.

3. **Stack depth**: 128K allocation might exceed stack mapping on some
   configs. Consider probing (touching each 4K page) or using a smaller
   buffer with a documented limit.

## Testing approach
Start with the SIMPLEST case: `echo "42" | ./bin` where bin is a scalar
number-output rule. Verify the output matches `./bin 42`. Then escalate to
multi-field, multi-record, text fields.

## Prerequisite (DONE)
The x86 decoder (validate_x86) now knows `cmp al, imm8` (0x3C) and
`mov r8, r/m8` (0x8A). Added before the stdin prologue shipped.
