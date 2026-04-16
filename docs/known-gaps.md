# Known Gaps in Native Backend

Gaps discovered through project-driven testing. Each is a real user-facing
limitation with a documented workaround. Ordered by impact.

## Text-valued let bindings

**Symptom**: `let sep = " | "` followed by `concat(... sep ...)` fails:
```
native codegen error: text literals not supported in native backend
```

**Root cause**: `emit_eval_expr` produces a scalar i64 in rax. Text values
are (ptr, len) pairs — they don't fit the "everything is rax" model. A
let binding evaluates its expression via emit_eval_expr and stores rax at
a rbp slot. Text literals can't go through that path.

**Workaround**: inline the text literal at each usage site instead of
binding it to a let. `concat(acc, " | ", e.name)` works; `let sep = " | "`
then `concat(acc, sep, e.name)` doesn't.

**Fix path**: Either (a) extend emit_eval_expr to handle text values by
storing (ptr, len) in TWO consecutive rbp slots (similar to Phase 2F's
err_ptr_slot/err_len_slot), or (b) detect text-typed let bindings at
compilation time and inline them at each reference site (constant
propagation). Option (b) is simpler for literals; option (a) is more
general (handles computed text values).

## Nested concat with Call args at 2+ levels

**Symptom**: `concat("a", outer_rule(p), "b")` where `outer_rule` body
is `concat("x", inner_rule(p), "y")` fails:
```
Phase 2H-b: nested concat cannot have its own Call args
```

**Root cause**: the `is_nested` flag in emit_concat_to_buffer_impl
prevents inner concats from having their own CallText pre-eval. The
outer's r11 slot base would be clobbered.

**Workaround**: flatten the composition by using an intermediate helper
rule that doesn't involve concat-of-Call, or restructure so Call appears
only at the top concat level.

**Fix path**: use rbp-relative slots for r11 saves instead of register
preservation. Requires prologue extension.
