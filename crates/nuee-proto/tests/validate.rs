use nuee_proto::{ValidateError, validate_module};

const EXPORTS: &str = r#"
  (func (export "nuee_alloc") (param i32) (result i32) i32.const 1024)
  (func (export "nuee_run") (param i32 i32) (result i64) i64.const 0)
"#;

fn module(body: &str) -> Vec<u8> {
    wat::parse_str(format!("(module {body})")).expect("valid wat")
}

fn task(memory: &str, extra: &str) -> Vec<u8> {
    module(&format!(
        r#"(memory (export "memory") {memory}) {EXPORTS} {extra}"#
    ))
}

#[test]
fn valid_task() {
    let info = validate_module(&task("1 16", "")).unwrap();
    assert_eq!(info.min_memory, 64 * 1024);
    assert_eq!(info.max_memory, 16 * 64 * 1024);
    // Allowed proposals: SIMD128, bulk memory, sign extension, saturating conversions.
    let simd = task(
        "1 16",
        r#"(func (param i32) (result i32)
             v128.const i32x4 1 2 3 4
             i32x4.extract_lane 0
             i32.extend8_s
             f32.const 1.5
             i32.trunc_sat_f32_s
             i32.add
             (memory.fill (i32.const 0) (i32.const 0) (i32.const 0)))"#,
    );
    assert!(validate_module(&simd).is_ok());
    // Exactly 1 GiB is allowed.
    assert!(validate_module(&task("1 16384", "")).is_ok());
}

#[test]
fn imports_are_refused() {
    let m = module(&format!(
        r#"(import "env" "log" (func)) (memory (export "memory") 1 2) {EXPORTS}"#
    ));
    assert_eq!(
        validate_module(&m),
        Err(ValidateError::HasImports {
            module: "env".into(),
            name: "log".into()
        })
    );
}

#[test]
fn memory_rules() {
    assert_eq!(
        validate_module(&task("1", "")),
        Err(ValidateError::NoMaxMemory)
    );
    assert!(matches!(
        validate_module(&task("1 16385", "")),
        Err(ValidateError::MemoryTooLarge(_))
    ));
    let none = module(EXPORTS);
    assert_eq!(validate_module(&none), Err(ValidateError::MemoryCount(0)));
}

#[test]
fn exports_and_signatures() {
    let no_mem_export = module(&format!("(memory 1 2) {EXPORTS}"));
    assert_eq!(
        validate_module(&no_mem_export),
        Err(ValidateError::MissingExport("memory"))
    );
    let no_run = module(
        r#"(memory (export "memory") 1 2)
           (func (export "nuee_alloc") (param i32) (result i32) i32.const 0)"#,
    );
    assert_eq!(
        validate_module(&no_run),
        Err(ValidateError::MissingExport("nuee_run"))
    );
    let bad = module(
        r#"(memory (export "memory") 1 2)
           (func (export "nuee_alloc") (param i32) (result i32) i32.const 0)
           (func (export "nuee_run") (param i32 i32) (result i32) i32.const 0)"#,
    );
    assert!(matches!(
        validate_module(&bad),
        Err(ValidateError::BadSignature {
            name: "nuee_run",
            ..
        })
    ));
}

#[test]
fn start_function_is_refused() {
    let m = task("1 2", "(func $s) (start $s)");
    assert_eq!(validate_module(&m), Err(ValidateError::HasStart));
}

#[test]
fn disallowed_features() {
    let relaxed = task(
        "1 2",
        r#"(func (result v128)
             (i8x16.relaxed_swizzle (v128.const i64x2 0 0) (v128.const i64x2 0 0)))"#,
    );
    assert!(matches!(
        validate_module(&relaxed),
        Err(ValidateError::Feature(_))
    ));
    let threads = task(
        "1 2",
        r#"(func (result i32) (i32.atomic.load (i32.const 0)))"#,
    );
    assert!(matches!(
        validate_module(&threads),
        Err(ValidateError::Feature(_))
    ));
    let shared = module(&format!(
        r#"(memory (export "memory") 1 2 shared) {EXPORTS}"#
    ));
    assert!(matches!(
        validate_module(&shared),
        Err(ValidateError::Feature(_))
    ));
    let tail = task("1 2", r#"(func $f (result i32) (return_call $f))"#);
    assert!(matches!(
        validate_module(&tail),
        Err(ValidateError::Feature(_))
    ));
}

#[test]
fn garbage_and_oversize() {
    assert!(matches!(
        validate_module(b"not wasm"),
        Err(ValidateError::Invalid(_))
    ));
    let big = vec![0u8; nuee_proto::limits::MAX_MODULE + 1];
    assert!(matches!(
        validate_module(&big),
        Err(ValidateError::TooLarge(_))
    ));
}
