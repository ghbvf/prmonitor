//! **Medium carrier (downstream side-door):** the primary launch funnel is Hard because production
//! consumers can only build managed subprocesses through opaque `ResolvedCli::command`. This
//! full-production-tree AST scan is the secondary machine guard: it recognizes process-command
//! aliases plus direct, wrapped, and statically bound managed executable names, and also rejects
//! deleted compatibility identifiers. Dynamic user-owned commands remain intentionally outside the
//! five-tool funnel, so this scan is Medium rather than a claimed type-system proof.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

use strum::IntoEnumIterator;
use syn::{
    visit::{self, Visit},
    Attribute, Block, Expr, ExprCall, File, Ident, Item, ItemFn, Lit, Local, Pat, UseTree,
};

use crate::model::CliTool;

#[test]
fn managed_clis_have_no_raw_production_launch_or_legacy_identifiers() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&root, &mut files);
    files.retain(|path| !path.ends_with(file!()));
    assert!(
        files.len() > 20,
        "guard must scan a non-empty production tree"
    );

    let mut scanned_bytes = 0usize;
    for path in files {
        let source = fs::read_to_string(&path).unwrap();
        scanned_bytes += source.len();
        let syntax: File = syn::parse_file(&source)
            .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()));
        let mut guard = ManagedCliGuard::new(&path);
        guard.visit_file(&syntax);
        assert!(
            guard.violations.is_empty(),
            "managed CLI guard found production side doors:\n{}",
            guard.violations.join("\n")
        );
    }
    assert!(
        scanned_bytes > 100_000,
        "guard scanned suspiciously little source"
    );
}

struct ManagedCliGuard<'a> {
    path: &'a Path,
    violations: Vec<String>,
    command_aliases: HashSet<String>,
    static_programs: HashMap<String, String>,
    managed_path_bindings: HashSet<String>,
}

impl<'a> ManagedCliGuard<'a> {
    fn new(path: &'a Path) -> Self {
        Self {
            path,
            violations: Vec::new(),
            command_aliases: HashSet::from(["Command".to_string(), "StdCommand".to_string()]),
            static_programs: HashMap::new(),
            managed_path_bindings: HashSet::new(),
        }
    }

    fn report(&mut self, detail: impl Into<String>) {
        self.violations
            .push(format!("{}: {}", self.path.display(), detail.into()));
    }
}

impl<'ast> Visit<'ast> for ManagedCliGuard<'_> {
    fn visit_item(&mut self, item: &'ast Item) {
        if item_attrs(item).is_some_and(has_cfg_test) {
            return;
        }
        if let Item::Use(item_use) = item {
            collect_command_aliases(&item_use.tree, &mut self.command_aliases);
        }
        if let Item::Const(item_const) = item {
            if let Some(program) = static_program(&item_const.expr, &self.static_programs) {
                self.static_programs
                    .insert(item_const.ident.to_string(), program);
            }
        }
        visit::visit_item(self, item);
    }

    fn visit_item_fn(&mut self, function: &'ast ItemFn) {
        let outer_aliases = self.command_aliases.clone();
        let outer_programs = self.static_programs.clone();
        let outer_paths = self.managed_path_bindings.clone();
        visit::visit_item_fn(self, function);
        self.command_aliases = outer_aliases;
        self.static_programs = outer_programs;
        self.managed_path_bindings = outer_paths;
    }

    fn visit_block(&mut self, block: &'ast Block) {
        let outer_aliases = self.command_aliases.clone();
        let outer_programs = self.static_programs.clone();
        let outer_paths = self.managed_path_bindings.clone();
        visit::visit_block(self, block);
        self.command_aliases = outer_aliases;
        self.static_programs = outer_programs;
        self.managed_path_bindings = outer_paths;
    }

    fn visit_local(&mut self, local: &'ast Local) {
        if let (Some(binding), Some(initializer)) = (local_binding(&local.pat), &local.init) {
            let name = binding.to_string();
            let program = static_program(&initializer.expr, &self.static_programs);
            let managed_path =
                is_managed_path_expression(&initializer.expr, &self.managed_path_bindings);
            // The initializer sees the prior binding, while the new binding takes effect only
            // after the statement. Visit first, then replace (or clear) same-name analysis state.
            visit::visit_local(self, local);
            self.static_programs.remove(&name);
            self.managed_path_bindings.remove(&name);
            if let Some(program) = program {
                self.static_programs.insert(name.clone(), program);
            }
            if managed_path {
                self.managed_path_bindings.insert(name);
            }
            return;
        }
        visit::visit_local(self, local);
    }

    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if is_raw_managed_cli_launch(
            call,
            &self.command_aliases,
            &self.static_programs,
            &self.managed_path_bindings,
        ) {
            self.report("raw Command::new(managed_cli) call");
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_ident(&mut self, ident: &'ast Ident) {
        if matches!(
            ident.to_string().as_str(),
            "CODEX_BIN" | "CLAUDE_BIN" | "cloudflared_bin" | "cloudflaredBin"
        ) {
            self.report(format!("legacy identifier `{ident}`"));
        }
        visit::visit_ident(self, ident);
    }
}

fn item_attrs(item: &Item) -> Option<&[Attribute]> {
    Some(match item {
        Item::Const(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::ExternCrate(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::ForeignMod(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::TraitAlias(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Use(item) => &item.attrs,
        _ => return None,
    })
}

fn has_cfg_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<Ident>()
                .is_ok_and(|ident| ident == "test")
    })
}

fn collect_command_aliases(tree: &UseTree, aliases: &mut HashSet<String>) {
    match tree {
        UseTree::Path(path) => collect_command_aliases(&path.tree, aliases),
        UseTree::Name(name) if name.ident == "Command" => {
            aliases.insert(name.ident.to_string());
        }
        UseTree::Rename(rename) if rename.ident == "Command" => {
            aliases.insert(rename.rename.to_string());
        }
        UseTree::Group(group) => {
            for item in &group.items {
                collect_command_aliases(item, aliases);
            }
        }
        _ => {}
    }
}

fn is_raw_managed_cli_launch(
    call: &ExprCall,
    command_aliases: &HashSet<String>,
    static_programs: &HashMap<String, String>,
    managed_path_bindings: &HashSet<String>,
) -> bool {
    let Expr::Path(function) = call.func.as_ref() else {
        return false;
    };
    let segments = function.path.segments.iter().collect::<Vec<_>>();
    if segments.len() < 2
        || !command_aliases.contains(&segments[segments.len() - 2].ident.to_string())
        || segments[segments.len() - 1].ident != "new"
    {
        return false;
    }
    let Some(argument) = call.args.first() else {
        return false;
    };
    static_program(argument, static_programs).is_some_and(|program| is_managed_program(&program))
        || is_managed_path_expression(argument, managed_path_bindings)
}

fn is_managed_path_expression(expr: &Expr, bindings: &HashSet<String>) -> bool {
    match expr {
        Expr::Path(path) if path.path.segments.len() == 1 => {
            bindings.contains(&path.path.segments[0].ident.to_string())
        }
        Expr::Field(field) => {
            matches!(
                &field.member,
                syn::Member::Named(name)
                    if matches!(
                        name.to_string().as_str(),
                        "configured_path"
                            | "resolved_path"
                            | "gh_path"
                            | "az_path"
                            | "codex_path"
                            | "claude_path"
                            | "cloudflared_path"
                    )
            ) || is_managed_path_expression(&field.base, bindings)
        }
        Expr::Call(call) => {
            let is_config_path_seam = matches!(call.func.as_ref(), Expr::Path(path)
            if path.path.segments.last().is_some_and(|segment| {
                matches!(segment.ident.to_string().as_str(), "configured_cli_path" | "diagnose_cli_from")
            }));
            is_config_path_seam
                || call
                    .args
                    .iter()
                    .any(|argument| is_managed_path_expression(argument, bindings))
        }
        Expr::MethodCall(call) => {
            call.method == "get_program"
                || is_managed_path_expression(&call.receiver, bindings)
                || call
                    .args
                    .iter()
                    .any(|argument| is_managed_path_expression(argument, bindings))
        }
        Expr::Group(group) => is_managed_path_expression(&group.expr, bindings),
        Expr::Paren(paren) => is_managed_path_expression(&paren.expr, bindings),
        Expr::Reference(reference) => is_managed_path_expression(&reference.expr, bindings),
        _ => false,
    }
}

fn static_program(expr: &Expr, bindings: &HashMap<String, String>) -> Option<String> {
    match expr {
        Expr::Lit(argument) => match &argument.lit {
            Lit::Str(program) => Some(program.value()),
            _ => None,
        },
        Expr::Path(path) if path.path.segments.len() == 1 => bindings
            .get(&path.path.segments[0].ident.to_string())
            .cloned(),
        Expr::Group(group) => static_program(&group.expr, bindings),
        Expr::Paren(paren) => static_program(&paren.expr, bindings),
        Expr::Reference(reference) => static_program(&reference.expr, bindings),
        Expr::MethodCall(call)
            if matches!(call.method.to_string().as_str(), "to_owned" | "to_string") =>
        {
            static_program(&call.receiver, bindings)
        }
        Expr::Call(call) if is_static_string_constructor(call.func.as_ref()) => call
            .args
            .first()
            .and_then(|arg| static_program(arg, bindings)),
        Expr::Call(call) if is_path_new(call.func.as_ref()) => call
            .args
            .first()
            .and_then(|arg| static_program(arg, bindings)),
        _ => None,
    }
}

fn local_binding(pattern: &Pat) -> Option<&Ident> {
    match pattern {
        Pat::Ident(binding) => Some(&binding.ident),
        Pat::Type(pattern) => local_binding(&pattern.pat),
        Pat::Paren(pattern) => local_binding(&pattern.pat),
        Pat::Reference(pattern) => local_binding(&pattern.pat),
        _ => None,
    }
}

fn is_path_new(function: &Expr) -> bool {
    let Expr::Path(function) = function else {
        return false;
    };
    let segments = function.path.segments.iter().collect::<Vec<_>>();
    segments.len() >= 2
        && segments
            .last()
            .is_some_and(|segment| segment.ident == "new")
        && segments[segments.len() - 2].ident == "Path"
}

fn is_static_string_constructor(function: &Expr) -> bool {
    let Expr::Path(function) = function else {
        return false;
    };
    let segments = function.path.segments.iter().collect::<Vec<_>>();
    segments.len() >= 2
        && segments
            .last()
            .is_some_and(|segment| segment.ident == "from")
        && matches!(
            segments[segments.len() - 2].ident.to_string().as_str(),
            "String" | "OsString" | "PathBuf"
        )
}

fn is_managed_program(program: &str) -> bool {
    let Some(file_name) = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    let lowercase = file_name.to_ascii_lowercase();
    let stem = [".exe", ".com", ".cmd", ".bat"]
        .into_iter()
        .find_map(|extension| lowercase.strip_suffix(extension))
        .unwrap_or(&lowercase);
    CliTool::iter().any(|tool| {
        serde_json::to_value(tool)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .is_some_and(|wire| wire == stem)
    })
}

fn collect_rs(directory: &Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_rs(&path, files);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            files.push(path);
        }
    }
}

#[test]
fn guard_keeps_scanning_production_items_after_a_test_only_item() {
    let syntax = syn::parse_file(
        r#"
        #[cfg(test)]
        fn helper() { std::process::Command::new("gh"); }

        fn production_side_door() { std::process::Command::new("codex"); }
        "#,
    )
    .unwrap();
    let mut guard = ManagedCliGuard::new(Path::new("fixture.rs"));
    guard.visit_file(&syntax);

    assert_eq!(guard.violations.len(), 1);
    assert!(guard.violations[0].contains("raw Command::new"));
}

#[test]
fn guard_does_not_leak_local_bindings_across_lexical_scopes() {
    let syntax = syn::parse_file(
        r#"
        fn first() { let program = "gh"; consume(program); }
        fn second(program: &str) { std::process::Command::new(program); }
        fn nested(dynamic: &str) {
            { let program = "codex"; consume(program); }
            std::process::Command::new(dynamic);
        }
        fn same_block_shadow(dynamic: &str) {
            let program = "claude";
            consume(program);
            let program = dynamic;
            std::process::Command::new(program);
        }
        fn local_alias_scope() { { use std::process::Command as Runner; consume(Runner); } }
        fn same_name_is_not_command() { Runner::new("gh"); }
        "#,
    )
    .unwrap();
    let mut guard = ManagedCliGuard::new(Path::new("scope-fixture.rs"));
    guard.visit_file(&syntax);

    assert!(guard.violations.is_empty(), "{:?}", guard.violations);
}

#[test]
fn guard_catches_wrapped_aliased_and_indirect_managed_cli_launches() {
    let syntax = syn::parse_file(
        r#"
        use tokio::process::Command as ProcessCommand;

        const GH_PROGRAM: &str = "/opt/tools/gh";

        fn production_side_doors() {
            let codex_program = String::from("codex");
            ProcessCommand::new(GH_PROGRAM);
            std::process::Command::new(codex_program);
            tokio::process::Command::new("az.exe".to_owned());
            std::process::Command::new(diagnostics.resolved_path);
        }
        "#,
    )
    .unwrap();
    let mut guard = ManagedCliGuard::new(Path::new("fixture.rs"));
    guard.visit_file(&syntax);

    assert_eq!(guard.violations.len(), 4, "{:?}", guard.violations);
    assert!(guard
        .violations
        .iter()
        .all(|violation| violation.contains("raw Command::new")));
}

#[test]
fn guard_catches_typed_bindings_and_path_new_wrappers() {
    let syntax = syn::parse_file(
        r#"
        fn production_side_doors() {
            let gh_program: &str = "gh";
            let codex_program = std::path::Path::new("codex");
            std::process::Command::new(gh_program);
            tokio::process::Command::new(codex_program);
        }
        "#,
    )
    .unwrap();
    let mut guard = ManagedCliGuard::new(Path::new("fixture.rs"));
    guard.visit_file(&syntax);

    assert_eq!(guard.violations.len(), 2, "{:?}", guard.violations);
}
