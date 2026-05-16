//! Visual preview of the styled permission prompt. Run with:
//!   cargo run -p aegis-core --example permission_preview
use aegis_core::PolicyPermission;
use serde_json::json;

fn main() {
    let cases = [
        (
            "edit_file",
            json!({
                "path": "src/main.rs",
                "old_string": "fn old()",
                "new_string": "fn new() {\n    println!(\"hi\");\n}",
            }),
        ),
        (
            "bash",
            json!({
                "command": "rm -rf target/",
            }),
        ),
    ];
    for (tool, args) in &cases {
        print!("{}", PolicyPermission::preview_box(tool, args, true));
    }
}
