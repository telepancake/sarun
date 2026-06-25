(module
  (import "host" "op" (func $op (result i32)))
  (memory (export "memory") 1)
  (func (export "run") (result i32)
    (call $op))
)
