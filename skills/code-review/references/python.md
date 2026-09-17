# Python review specifics

- Mutable default args (`def f(x=[])`), late-binding closures in loops.
- Bare `except:` / `except Exception: pass` — swallowed errors.
- `eval`/`exec`/`pickle.loads`/`subprocess(shell=True)` on external data.
- Resource handling: files/sockets without `with`, `finally` that can't run.
- Mutating a collection while iterating it; `dict` key assumptions.
- Type confusion on `None` returns — is every caller handling it?
- `==` vs `is` on values that aren't singletons.
- Async: `await` forgotten, blocking call inside coroutine, task created
  without keeping a reference (GC'd mid-flight).
