# Design Lessons (R&D journal)

Hard-won insights from building a language and compiler that don't exist yet. These aren't rules — they're documented scars. Read them before proposing a large change; they'll save you a round of debugging.

## Lock the design before writing code ("bien réfléchir en amont")

Every native backend phase that went smoothly (3, 4, 5a, 5b, 2F, 2G) had its design committed to CLAUDE.md BEFORE implementation. Every phase where we dove in first (2H-b's initial push/pop approach) hit a runtime bug that the design step would have caught. The pattern: write the emission flow pseudocode, identify register lifetimes, check for nesting issues, THEN implement. The design commit is cheap insurance — ~30 minutes of thinking saves hours of SIGSEGV debugging.

## Rejection notes go stale fast

When the infrastructure evolves (new helpers, refactored prologues, new ConcatArgKind variants), the "What native still rejects" list drifts. Three times in a single session we found rejection notes that were already stale — the feature worked but the doc said it didn't. **Test the claim before believing it.** When you fix a rejection, add a regression test that locks the working behavior so the claim can't drift back.

## Pointers are inevitable but not exposed

At the machine-code level, every text value is a pointer. There's no way to emit x86-64 text handling without addresses in registers. The discipline is:
- **Provenance is closed**: pointers come from three sources only — argv (kernel-provided), rbp slots (we wrote them), rsp-allocated buffers (we sub'd for them). No arbitrary addresses.
- **The language doesn't expose them**: users write `text`, not `*text`. The (ptr, len) representation is an emitter internal.
- **No pointer arithmetic in emitted code**: the only operations are load/store at constant offsets and `rep movsb` with bounded length. No `p + n` or `*(p + offset)` with user-controlled offset.

This is philosophically different from C: same CPU instruction, different trust model.

## The EB-heuristic lesson (validator)

The x86 decoder's jmp-over-data heuristic originally treated ALL `EB xx` as "skip xx bytes of data". A backward jump (`EB E5`, disp = -27 as i8) was read as "skip 229 bytes forward", landing mid-instruction. Root cause: unsigned vs signed byte interpretation. Fix: 2 lines (cast to i8, only skip when positive). Lesson: any heuristic that processes compiler output must understand the FULL range of what the compiler emits, not just the common case.

## Buffer lifetime across nested concats (Phase 2H-b)

When a concat arg is a rule call whose body is itself a concat, the inner concat allocates a buffer by `sub rsp, N`. If the outer saved state via `push reg` before calling the inner, the inner's allocation moves rsp BELOW the push location — making `pop reg` read garbage from inside the inner's buffer. The fix was architectural: `is_nested` flag makes the inner skip its own `mov r9, rsp` (so the outer's r9 survives as a register value) and refuse its own CallText args (one-level-of-pre-eval scope restriction). The general lesson: in a stack-only memory model without a frame-pointer convention between "functions" (which are really inline code blocks), save/restore via push/pop is fragile whenever the callee allocates dynamically. Use register preservation or rbp-relative slots instead.

## Stale doc drift is a credibility issue

The project's identity is "explicit + verified + optimized". If CLAUDE.md says a feature is rejected but it actually works, that's the same class of error as a false positive in the verifier — it damages trust. When you ship a feature, update the rejection list AND the phase table in the SAME commit. Don't defer the doc update "for later".
