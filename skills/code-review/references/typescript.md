# TypeScript review specifics

- `any`/`as` casts silencing the type system — what's the real shape?
- Promise without `await`/`return`/`catch` — floating rejections.
- `JSON.parse` on external data without validation — typed lie.
- Effect cleanup missing (React `useEffect`, subscriptions, timers).
- Stale closures capturing old state in async callbacks/handlers.
- `undefined`/`null` paths — optional chaining where a real check is needed.
- Array/object mutation shared across renders or between components.
- `number` precision on money/ids; `==` vs `===` on user input.
- Race: two async operations updating the same state out of order.
