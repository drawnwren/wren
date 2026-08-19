(component
    (core module $m
        (memory (export "memory") 1)
        (func (export "completion-count") (param i32) (result i32)
            local.get 0
            i32.const 3
            i32.add)
        (func (export "decoration-count") (param i32) (result i32)
            local.get 0
            i32.const 2
            i32.rem_u
            i32.const 1
            i32.add)
        (func (export "burn")
            (loop $forever
                br $forever))
    )
    (core instance $i (instantiate $m))
    (func (export "completion-count")
        (param "prefix-bytes" u32)
        (result u32)
        (canon lift (core func $i "completion-count")))
    (func (export "decoration-count")
        (param "visible-ranges" u32)
        (result u32)
        (canon lift (core func $i "decoration-count")))
    (func (export "burn")
        (canon lift (core func $i "burn")))
)
