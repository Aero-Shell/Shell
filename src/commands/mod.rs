pub mod aepkg;
pub mod builtins;

use crate::registry;

/// Register the regular builtins. The special builtins (`exec`, `eval`,
/// `set`, `trap`, …) live in [`crate::shell::special`] because they need the
/// shell's own state and control flow.
pub fn register_builtins() {
    use registry::register;

    // Session state
    register("history", builtins::history);
    register("alias", builtins::alias);
    register("type", builtins::type_cmd);
    register("which", builtins::type_cmd);

    // Core file/dir operations
    register("ls", builtins::ls);
    register("cd", builtins::cd);
    register("mkdir", builtins::mkdir);
    register("pwd", builtins::pwd);

    // Shell management
    register("clear", builtins::clear);
    register("help", builtins::help);
    register("ver", builtins::ver);
    register("aepkg", aepkg::aepkg);
}
