#[must_use]
pub fn render_cli_dispatcher(version: &str, runtime_specifier: &str) -> String {
    format!(
        "#!/usr/bin/env npx tsx\n\
const args = process.argv.slice(2);\n\
if (args.length === 1 && (args[0] === '--version' || args[0] === '-v' || args[0] === '-V')) {{\n\
  console.log('{version} (Claude Code)');\n\
}} else {{\n\
  await import('{runtime_specifier}');\n\
}}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::render_cli_dispatcher;

    #[test]
    fn dispatcher_does_not_static_import_runtime() {
        let dispatcher = render_cli_dispatcher("2.1.76", "./index.runtime.js");

        assert!(dispatcher.contains("await import('./index.runtime.js')"));
        assert!(
            !dispatcher
                .lines()
                .any(|line| line.trim_start().starts_with("import "))
        );
        assert!(dispatcher.contains("'--version'"));
    }
}
