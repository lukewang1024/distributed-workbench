// A distinct Mach-O identity for macOS privacy prompts. It intentionally uses
// the same typed CLI/runtime implementation as `workbench` while never
// involving an interpreter or shell wrapper.
include!("../main.rs");
