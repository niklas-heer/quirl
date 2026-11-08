//! Native execution of compound POSIX programs.
//!
//! [`NativeExecutor::execute_program`] walks a [`quirl_syntax::Program`] in
//! the session's own process: lists, `&&`/`||`, negation, `if`, `for`,
//! `while`, `until`, `case`, brace groups, and function definitions. Every
//! simple pipeline still runs through the executor's existing expansion,
//! built-in, and process path, so job control, terminal handoff, capture
//! limits, and cancellation behave exactly as for a single command line.
//!
//! Failure model: an operating error from any command (a spawn failure, an
//! exceeded limit, cancellation) aborts the whole program and is returned
//! unchanged; session state already committed by earlier commands, such as
//! variables and functions, stays committed, as it would in any shell.
//! Function calls always restore positional parameters, `local` variables,
//! and the call depth, including when the body fails.
//!
//! The walk recurses once per nested compound command and once per function
//! call. Parsing bounds compound nesting by
//! [`quirl_syntax::PROGRAM_NESTING_MAX`]; calls are bounded by
//! [`FUNCTION_CALL_DEPTH_MAX`]. Loops have no iteration limit, but every
//! iteration observes request cancellation and the host interrupt flag.

use super::{
    NativeExecutor, OutputObserverHandle, RequestContext, append_captured_output,
    finish_control_builtin, outcome, retained_output_limit, validate_control_redirects,
};
use quirl_core::{CommandOutcome, ErrorCode, ShellError};
use quirl_syntax::{
    CaseClause, CommandNode, CompoundCommand, CompoundList, ForClause, FunctionDefinition,
    IfClause, ListConnector, LoopClause, Pipeline, PipelineNode, Program, Quoting, Redirect,
    RedirectKind, RedirectedCompound, SimpleCommand, Word, WordPart,
};
use std::{ffi::OsString, sync::Arc, sync::atomic::Ordering};

/// Deepest chain of nested function calls.
pub(crate) const FUNCTION_CALL_DEPTH_MAX: usize = 100;
/// Most functions one session may define.
pub(crate) const FUNCTIONS_MAX: usize = 1024;
/// Most aliases one session may define.
pub(crate) const ALIASES_MAX: usize = 1024;
/// Largest function definition or alias value, in bytes.
pub(crate) const DEFINITION_BYTES_MAX: usize = 64 * 1024;
/// Most positional parameters one function call may receive.
const POSITIONAL_PARAMETERS_MAX: usize = 65_536;
/// Status a shell reports for a command ended by `SIGINT`.
const INTERRUPTED_STATUS: i32 = 130;

/// Commands that run inside the executor without starting a process.
pub(crate) const NATIVE_BUILTINS: [&str; 14] = [
    "true", "false", ":", "echo", "test", "[", "alias", "unalias", "type", "local", "shift",
    "return", "break", "continue",
];

/// How control leaves a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    /// Continue with the next command.
    Next,
    /// `break N`: leave N enclosing loops.
    Break(u32),
    /// `continue N`: resume the Nth enclosing loop.
    Continue(u32),
    /// `return`: leave the current function.
    Return,
    /// Stop the whole program, as after Ctrl-C.
    Abort,
}

/// One `local` variable's state before the function changed it.
pub(crate) struct SavedLocal {
    name: String,
    previous: Option<(OsString, bool)>,
}

/// Per-program execution state threaded through the walk.
struct RunContext<'s, 'r, 'o, 'h> {
    source: &'s str,
    capture: bool,
    request: Option<RequestContext<'r>>,
    observer: Option<&'o OutputObserverHandle<'h>>,
    /// Status of the most recent command, `$?` for the next one.
    status: i32,
    stdout: String,
    stderr: String,
    /// Outcome of the most recent simple pipeline, returned for inherited
    /// output so hosts see the same shape as before compound support.
    last: Option<CommandOutcome>,
}

impl RunContext<'_, '_, '_, '_> {
    fn record(&mut self, result: CommandOutcome) -> Result<(), ShellError> {
        self.status = result.status;
        if self.capture {
            let limit = retained_output_limit(self.request.map(|request| request.request));
            append_captured_output(
                &mut self.stdout,
                result.stdout.as_deref().unwrap_or_default(),
                limit,
            )?;
            append_captured_output(
                &mut self.stderr,
                result.stderr.as_deref().unwrap_or_default(),
                limit,
            )?;
        }
        self.last = Some(result);
        Ok(())
    }
}

impl NativeExecutor {
    /// Execute a parsed program and return its final status and, when
    /// `capture` is set, everything its commands wrote.
    pub(super) fn execute_program(
        &mut self,
        program: &Program,
        source: &str,
        capture: bool,
        request: Option<RequestContext<'_>>,
        observer: Option<&OutputObserverHandle<'_>>,
    ) -> Result<CommandOutcome, ShellError> {
        let mut context = RunContext {
            source,
            capture,
            request,
            observer,
            status: self.previous_status,
            stdout: String::new(),
            stderr: String::new(),
            last: None,
        };
        let enclosing_flow = self.pending_flow.take();
        let result = self.run_list(&program.body, &mut context);
        // A nested program, such as a command substitution, must not leak a
        // `break` or `return` into the command that contains it.
        self.pending_flow = enclosing_flow;
        result?;
        let mut last = context
            .last
            .take()
            .unwrap_or_else(|| outcome(context.status, None, None));
        last.status = context.status;
        if capture {
            last.stdout = Some(context.stdout);
            last.stderr = Some(context.stderr);
        }
        Ok(last)
    }

    fn run_list(
        &mut self,
        list: &CompoundList,
        context: &mut RunContext<'_, '_, '_, '_>,
    ) -> Result<Flow, ShellError> {
        for item in &list.items {
            if self.interrupted(context)? {
                return Ok(Flow::Abort);
            }
            let first = &item.and_or.first;
            let mut flow = self.run_pipeline_node(first, item.background, context)?;
            for next in &item.and_or.rest {
                if flow != Flow::Next {
                    break;
                }
                let skip = match next.connector {
                    ListConnector::And => context.status != 0,
                    ListConnector::Or => context.status == 0,
                    ListConnector::Sequence => false,
                };
                if !skip {
                    flow = self.run_pipeline_node(&next.pipeline, false, context)?;
                }
            }
            if flow != Flow::Next {
                return Ok(flow);
            }
        }
        Ok(Flow::Next)
    }

    /// Report whether the host asked to stop, or the request was cancelled.
    fn interrupted(&self, context: &mut RunContext<'_, '_, '_, '_>) -> Result<bool, ShellError> {
        if let Some(request) = context.request {
            request.ensure_active()?;
        }
        let interrupted = self
            .interrupt
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed));
        if interrupted {
            context.status = INTERRUPTED_STATUS;
        }
        Ok(interrupted)
    }

    fn run_pipeline_node(
        &mut self,
        node: &PipelineNode,
        background: bool,
        context: &mut RunContext<'_, '_, '_, '_>,
    ) -> Result<Flow, ShellError> {
        let flow = match node.commands.as_slice() {
            [CommandNode::Compound(compound)] => self.run_compound(compound, context)?,
            [CommandNode::RedirectedCompound(redirected)] => {
                self.run_redirected_compound(redirected, context)?
            }
            [CommandNode::FunctionDefinition(definition)] => {
                self.define_function(definition)?;
                context.status = 0;
                Flow::Next
            }
            commands => {
                let mut simple = Vec::with_capacity(commands.len());
                for command in commands {
                    let CommandNode::Simple(command) = command else {
                        return Err(ShellError::new(
                            ErrorCode::InvalidCommand,
                            "a compound command cannot be a pipeline stage natively",
                        )
                        .with_help("Run the pipeline in `bash { ... }` or `zsh { ... }`"));
                    };
                    let mut command = command.clone();
                    if !self.inherited_redirects.is_empty() {
                        // Outer redirections apply first, as the shell's own
                        // descriptors would, so the command's own come after.
                        command
                            .redirects
                            .splice(0..0, self.inherited_redirects.iter().cloned());
                    }
                    simple.push(command);
                }
                let pipeline = Pipeline {
                    commands: simple,
                    background,
                };
                let result = self.execute_pipeline(
                    &pipeline,
                    context.source,
                    context.capture,
                    context.request,
                    context.observer,
                    context.status,
                )?;
                context.record(result)?;
                match self.pending_flow.take() {
                    Some(flow) => flow,
                    None if context.status == INTERRUPTED_STATUS && !background => Flow::Abort,
                    None => Flow::Next,
                }
            }
        };
        if node.negated {
            context.status = i32::from(context.status == 0);
        }
        Ok(flow)
    }

    /// Run a compound command whose output is redirected. Each target is
    /// opened once, truncating for `>`, and every command in the body then
    /// appends to it, so output streams exactly as through one descriptor.
    /// Only regular files and character devices qualify: reopening a FIFO or
    /// socket per command would not behave like one shared descriptor.
    fn run_redirected_compound(
        &mut self,
        redirected: &RedirectedCompound,
        context: &mut RunContext<'_, '_, '_, '_>,
    ) -> Result<Flow, ShellError> {
        let mut inherited = Vec::with_capacity(redirected.redirects.len());
        for redirect in &redirected.redirects {
            if redirect.kind == RedirectKind::DuplicateOutput {
                inherited.push(redirect.clone());
                continue;
            }
            let path = self.expand_single_word(&redirect.target, context)?;
            let absolute = std::env::current_dir()
                .map_err(|error| {
                    ShellError::new(ErrorCode::Io, "could not resolve the redirection path")
                        .with_context(error.to_string())
                        .with_help("Enter an existing directory and retry")
                })?
                .join(&path);
            let absolute = absolute.to_str().map(str::to_owned).ok_or_else(|| {
                ShellError::new(
                    ErrorCode::InvalidArgument,
                    "the redirection path is not valid UTF-8",
                )
                .with_help("Redirect to a path with a UTF-8 name")
            })?;
            let opened = Redirect {
                fd: redirect.fd,
                kind: redirect.kind,
                path: absolute.clone(),
                target: literal_word(&absolute),
            };
            let file = super::open_redirected_output(&opened, context.request)?;
            let file_type = file
                .metadata()
                .map_err(|error| {
                    ShellError::new(ErrorCode::Io, "could not inspect the redirection target")
                        .with_context(error.to_string())
                        .with_help("Redirect to a regular file")
                })?
                .file_type();
            let shareable =
                file_type.is_file() || std::os::unix::fs::FileTypeExt::is_char_device(&file_type);
            if !shareable {
                return Err(ShellError::new(
                    ErrorCode::InvalidCommand,
                    "a compound command can only redirect to a file or device natively",
                )
                .with_context(format!("target {path}"))
                .with_help(
                    "Run the command in `bash { ... }` to redirect it to a pipe or socket",
                ));
            }
            inherited.push(Redirect {
                kind: RedirectKind::Append,
                ..opened
            });
        }
        let depth = self.inherited_redirects.len();
        self.inherited_redirects.extend(inherited);
        let result = self.run_compound(&redirected.command, context);
        self.inherited_redirects.truncate(depth);
        result
    }

    fn run_compound(
        &mut self,
        compound: &CompoundCommand,
        context: &mut RunContext<'_, '_, '_, '_>,
    ) -> Result<Flow, ShellError> {
        match compound {
            CompoundCommand::BraceGroup(list) => self.run_list(list, context),
            CompoundCommand::If(clause) => self.run_if(clause, context),
            CompoundCommand::For(clause) => {
                self.loop_depth = self.loop_depth.saturating_add(1);
                let result = self.run_for(clause, context);
                self.loop_depth = self.loop_depth.saturating_sub(1);
                result
            }
            CompoundCommand::While(clause) => {
                self.loop_depth = self.loop_depth.saturating_add(1);
                let result = self.run_loop(clause, true, context);
                self.loop_depth = self.loop_depth.saturating_sub(1);
                result
            }
            CompoundCommand::Until(clause) => {
                self.loop_depth = self.loop_depth.saturating_add(1);
                let result = self.run_loop(clause, false, context);
                self.loop_depth = self.loop_depth.saturating_sub(1);
                result
            }
            CompoundCommand::Case(clause) => self.run_case(clause, context),
        }
    }

    fn run_if(
        &mut self,
        clause: &IfClause,
        context: &mut RunContext<'_, '_, '_, '_>,
    ) -> Result<Flow, ShellError> {
        for branch in &clause.branches {
            let flow = self.run_list(&branch.condition, context)?;
            if flow != Flow::Next {
                return Ok(flow);
            }
            if context.status == 0 {
                return self.run_list(&branch.body, context);
            }
        }
        if let Some(body) = &clause.else_body {
            return self.run_list(body, context);
        }
        context.status = 0;
        Ok(Flow::Next)
    }

    fn run_for(
        &mut self,
        clause: &ForClause,
        context: &mut RunContext<'_, '_, '_, '_>,
    ) -> Result<Flow, ShellError> {
        let values = match &clause.words {
            Some(words) => self.expand_word_list(words, context)?,
            None => self.positional.clone(),
        };
        context.status = 0;
        for value in values {
            if self.interrupted(context)? {
                return Ok(Flow::Abort);
            }
            self.environment
                .assign_variables(&[(clause.variable.clone(), value)])?;
            match loop_step(self.run_list(&clause.body, context)?) {
                LoopStep::Continue => {}
                LoopStep::Exit(flow) => return Ok(flow),
            }
        }
        Ok(Flow::Next)
    }

    fn run_loop(
        &mut self,
        clause: &LoopClause,
        while_loop: bool,
        context: &mut RunContext<'_, '_, '_, '_>,
    ) -> Result<Flow, ShellError> {
        let mut body_status = 0;
        loop {
            if self.interrupted(context)? {
                return Ok(Flow::Abort);
            }
            match loop_step(self.run_list(&clause.condition, context)?) {
                LoopStep::Continue => {}
                LoopStep::Exit(flow) => return Ok(flow),
            }
            if (context.status == 0) != while_loop {
                break;
            }
            let step = loop_step(self.run_list(&clause.body, context)?);
            body_status = context.status;
            if let LoopStep::Exit(flow) = step {
                return Ok(flow);
            }
        }
        context.status = body_status;
        Ok(Flow::Next)
    }

    fn run_case(
        &mut self,
        clause: &CaseClause,
        context: &mut RunContext<'_, '_, '_, '_>,
    ) -> Result<Flow, ShellError> {
        let subject = self.expand_single_word(&clause.subject, context)?;
        for arm in &clause.arms {
            for pattern in &arm.patterns {
                let pattern = self.expand_case_pattern(pattern, context)?;
                if case_pattern_matches(&pattern, &subject) {
                    context.status = 0;
                    return self.run_list(&arm.body, context);
                }
            }
        }
        context.status = 0;
        Ok(Flow::Next)
    }

    fn expand_word_list(
        &mut self,
        words: &[Word],
        context: &RunContext<'_, '_, '_, '_>,
    ) -> Result<Vec<String>, ShellError> {
        let mut budget = super::ExpansionBudget::new();
        let mut values = Vec::new();
        for word in words {
            values.extend(self.expand_word_fields(
                word,
                context.request,
                context.status,
                &mut budget,
            )?);
        }
        Ok(values)
    }

    fn expand_single_word(
        &mut self,
        word: &Word,
        context: &RunContext<'_, '_, '_, '_>,
    ) -> Result<String, ShellError> {
        let mut budget = super::ExpansionBudget::new();
        Ok(self
            .expand_word(
                word,
                super::SUBSTITUTION_BYTES_MAX,
                context.request,
                context.status,
                &mut budget,
            )?
            .value)
    }

    /// Expand a `case` pattern into characters and whether each is literal.
    /// Quoted and escaped text matches itself; unquoted text, including the
    /// result of unquoted expansions, keeps its pattern meaning.
    fn expand_case_pattern(
        &mut self,
        word: &Word,
        context: &RunContext<'_, '_, '_, '_>,
    ) -> Result<Vec<(char, bool)>, ShellError> {
        let mut budget = super::ExpansionBudget::new();
        let mut pattern = Vec::new();
        for part in &word.parts {
            let literal = part.quoting != Quoting::Unquoted;
            let text = match part.quoting {
                Quoting::Single | Quoting::Escaped => part.text.clone(),
                Quoting::Double | Quoting::Unquoted => {
                    let mut expanded = String::new();
                    self.expand_fragment(
                        &part.text,
                        super::SUBSTITUTION_BYTES_MAX,
                        context.request,
                        context.status,
                        &mut expanded,
                        &mut budget,
                        None,
                        part.quoting == Quoting::Double,
                    )?;
                    expanded
                }
            };
            pattern.extend(text.chars().map(|character| (character, literal)));
        }
        Ok(pattern)
    }

    fn define_function(&mut self, definition: &FunctionDefinition) -> Result<(), ShellError> {
        if definition.source.len() > DEFINITION_BYTES_MAX {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                format!("function `{}` is too large to define", definition.name),
            )
            .with_context(format!(
                "limit {DEFINITION_BYTES_MAX} bytes; observed {} bytes",
                definition.source.len()
            ))
            .with_help("Split the function into smaller functions or move it to a script"));
        }
        if !self.functions.contains_key(&definition.name) && self.functions.len() >= FUNCTIONS_MAX {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "the session has too many functions",
            )
            .with_context(format!("limit {FUNCTIONS_MAX} functions"))
            .with_help("Remove unused functions with `unset -f name`"));
        }
        self.functions
            .insert(definition.name.clone(), Arc::new(definition.clone()));
        self.note_session_commands_changed();
        Ok(())
    }

    /// Advance the generation that tells hosts the function or alias set changed.
    fn note_session_commands_changed(&mut self) {
        self.session_commands_generation = self.session_commands_generation.saturating_add(1);
    }

    /// Run a function for an expanded single-command pipeline.
    ///
    /// Output redirections apply to the whole body, which then runs captured
    /// so its output can be written to the file. Input redirections would
    /// need a subshell and are rejected.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the executor's pipeline entry points, which thread these values explicitly"
    )]
    pub(super) fn call_function(
        &mut self,
        definition: &FunctionDefinition,
        command: &SimpleCommand,
        source: &str,
        capture: bool,
        request: Option<RequestContext<'_>>,
        observer: Option<&OutputObserverHandle<'_>>,
        previous_status: i32,
    ) -> Result<CommandOutcome, ShellError> {
        if command.redirects.iter().any(|redirect| {
            matches!(
                redirect.kind,
                quirl_syntax::RedirectKind::Input | quirl_syntax::RedirectKind::HereString
            )
        }) {
            return Err(ShellError::new(
                ErrorCode::InvalidCommand,
                format!(
                    "function `{}` cannot read redirected input natively",
                    definition.name
                ),
            )
            .with_command(source)
            .with_help("Pipe the input to a script, or run the call in `bash { ... }`"));
        }
        if self.call_depth >= FUNCTION_CALL_DEPTH_MAX {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "function calls nest too deeply",
            )
            .with_context(format!(
                "limit {FUNCTION_CALL_DEPTH_MAX} nested calls; calling `{}`",
                definition.name
            ))
            .with_help("Check for a function that calls itself without a stopping condition"));
        }
        let arguments = command.words.get(1..).unwrap_or_default();
        if arguments.len() > POSITIONAL_PARAMETERS_MAX {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "a function call has too many arguments",
            )
            .with_context(format!("limit {POSITIONAL_PARAMETERS_MAX} arguments"))
            .with_help("Pass large data through a file"));
        }
        validate_control_redirects(command, request)?;
        let redirected = !command.redirects.is_empty();
        let mut context = RunContext {
            source,
            capture: capture || redirected,
            request,
            observer: if redirected { None } else { observer },
            status: previous_status,
            stdout: String::new(),
            stderr: String::new(),
            last: None,
        };
        let saved_positional = std::mem::replace(&mut self.positional, arguments.to_vec());
        self.call_depth = self.call_depth.saturating_add(1);
        self.local_frames.push(Vec::new());
        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
        let result = self.run_compound(&definition.body, &mut context);
        self.loop_depth = saved_loop_depth;
        let restored = self.restore_locals();
        self.call_depth = self.call_depth.saturating_sub(1);
        self.positional = saved_positional;
        let flow = result?;
        restored?;
        if flow == Flow::Abort {
            self.pending_flow = Some(Flow::Abort);
        }
        let body = outcome(
            context.status,
            context.capture.then_some(context.stdout),
            context.capture.then_some(context.stderr),
        );
        if redirected {
            return finish_control_builtin(command, body, capture, request);
        }
        Ok(body)
    }

    fn restore_locals(&mut self) -> Result<(), ShellError> {
        let Some(frame) = self.local_frames.pop() else {
            return Ok(());
        };
        let mut first_error = None;
        for saved in frame.into_iter().rev() {
            if let Err(error) = self
                .environment
                .restore_variable(&saved.name, saved.previous)
            {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Run a built-in from [`NATIVE_BUILTINS`] for an expanded single command,
    /// or return `None` when `command` names something else.
    pub(super) fn execute_native_builtin(
        &mut self,
        command: &SimpleCommand,
        capture: bool,
        request: Option<RequestContext<'_>>,
        previous_status: i32,
    ) -> Result<Option<CommandOutcome>, ShellError> {
        let Some(name) = command.words.first().map(String::as_str) else {
            return Ok(None);
        };
        if !NATIVE_BUILTINS.contains(&name) {
            return Ok(None);
        }
        validate_control_redirects(command, request)?;
        let operands = command.words.get(1..).unwrap_or_default();
        let result = match name {
            "true" | ":" => status_only(0),
            "false" => status_only(1),
            "echo" => {
                let bytes = crate::echo_builtin::render_echo(
                    &command.words,
                    retained_output_limit(request.map(|request| request.request)),
                )?;
                match String::from_utf8(bytes) {
                    Ok(text) => outcome(0, Some(text), Some(String::new())),
                    Err(error) => {
                        return write_raw_echo(command, error.into_bytes(), capture, request)
                            .map(Some);
                    }
                }
            }
            "test" | "[" => match crate::test_builtin::evaluate_test(&command.words) {
                Ok(crate::test_builtin::TestOutcome::True) => status_only(0),
                Ok(crate::test_builtin::TestOutcome::False) => status_only(1),
                Err(error) => failure(2, name, &error.message),
            },
            "alias" => self.alias_builtin(operands)?,
            "unalias" => self.unalias_builtin(operands),
            "type" => self.type_builtin(operands),
            "local" => self.local_builtin(operands)?,
            "shift" => self.shift_builtin(operands),
            "return" => self.return_builtin(operands, previous_status),
            "break" | "continue" => self.loop_control_builtin(name, operands),
            _ => return Ok(None),
        };
        finish_control_builtin(command, result, capture, request).map(Some)
    }

    fn alias_builtin(&mut self, operands: &[String]) -> Result<CommandOutcome, ShellError> {
        if operands.is_empty() {
            let listing = self
                .aliases
                .iter()
                .map(|(name, value)| format!("alias {name}={}\n", single_quote(value)))
                .collect::<String>();
            return Ok(outcome(0, Some(listing), Some(String::new())));
        }
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut status = 0;
        for operand in operands {
            match operand.split_once('=') {
                Some((name, value)) => {
                    if !is_alias_name(name) {
                        stderr.push_str(&format!("alias: `{name}`: invalid alias name\n"));
                        status = 1;
                        continue;
                    }
                    if value.len() > DEFINITION_BYTES_MAX {
                        return Err(ShellError::new(
                            ErrorCode::ResourceLimit,
                            format!("alias `{name}` is too long"),
                        )
                        .with_context(format!("limit {DEFINITION_BYTES_MAX} bytes"))
                        .with_help("Define a function instead"));
                    }
                    if !self.aliases.contains_key(name) && self.aliases.len() >= ALIASES_MAX {
                        return Err(ShellError::new(
                            ErrorCode::ResourceLimit,
                            "the session has too many aliases",
                        )
                        .with_context(format!("limit {ALIASES_MAX} aliases"))
                        .with_help("Remove unused aliases with `unalias name`"));
                    }
                    self.aliases.insert(name.to_owned(), value.to_owned());
                    self.note_session_commands_changed();
                }
                None => match self.aliases.get(operand) {
                    Some(value) => {
                        stdout.push_str(&format!("alias {operand}={}\n", single_quote(value)));
                    }
                    None => {
                        stderr.push_str(&format!("alias: {operand}: not found\n"));
                        status = 1;
                    }
                },
            }
        }
        Ok(outcome(status, Some(stdout), Some(stderr)))
    }

    fn unalias_builtin(&mut self, operands: &[String]) -> CommandOutcome {
        if operands.is_empty() {
            return failure(2, "unalias", "usage: unalias [-a] name...");
        }
        if operands.iter().any(|operand| operand == "-a") {
            if !self.aliases.is_empty() {
                self.aliases.clear();
                self.note_session_commands_changed();
            }
            return status_only(0);
        }
        let mut stderr = String::new();
        for operand in operands {
            match self.aliases.remove(operand) {
                Some(_) => self.note_session_commands_changed(),
                None => stderr.push_str(&format!("unalias: {operand}: not found\n")),
            }
        }
        outcome(
            i32::from(!stderr.is_empty()),
            Some(String::new()),
            Some(stderr),
        )
    }

    fn type_builtin(&self, operands: &[String]) -> CommandOutcome {
        let mut stdout = String::new();
        let mut stderr = String::new();
        for name in operands {
            if let Some(value) = self.aliases.get(name) {
                stdout.push_str(&format!("{name} is an alias for {value}\n"));
            } else if quirl_syntax::is_reserved_word(name) {
                stdout.push_str(&format!("{name} is a shell keyword\n"));
            } else if let Some(definition) = self.functions.get(name) {
                stdout.push_str(&format!("{name} is a function\n{}\n", definition.source));
            } else if is_shell_builtin(name) {
                stdout.push_str(&format!("{name} is a shell builtin\n"));
            } else if let Some(path) = self.environment.resolve_executable(name) {
                stdout.push_str(&format!("{name} is {}\n", path.display()));
            } else {
                stderr.push_str(&format!("type: {name}: not found\n"));
            }
        }
        outcome(i32::from(!stderr.is_empty()), Some(stdout), Some(stderr))
    }

    fn local_builtin(&mut self, operands: &[String]) -> Result<CommandOutcome, ShellError> {
        if self.local_frames.is_empty() {
            return Ok(failure(1, "local", "can only be used in a function"));
        }
        for operand in operands {
            let (name, value) = match operand.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (operand.as_str(), None),
            };
            if !quirl_syntax::is_shell_name(name) {
                return Ok(failure(
                    1,
                    "local",
                    &format!("`{name}`: not a valid identifier"),
                ));
            }
            let already_saved = self
                .local_frames
                .last()
                .is_some_and(|frame| frame.iter().any(|saved| saved.name == name));
            if !already_saved {
                let previous = self.environment.saved_variable(name);
                if let Some(frame) = self.local_frames.last_mut() {
                    frame.push(SavedLocal {
                        name: name.to_owned(),
                        previous,
                    });
                }
            }
            match value {
                Some(value) => self
                    .environment
                    .restore_variable(name, Some((OsString::from(value), false)))?,
                None => self.environment.unset_names(&[name.to_owned()])?,
            }
        }
        Ok(status_only(0))
    }

    fn shift_builtin(&mut self, operands: &[String]) -> CommandOutcome {
        let count = match operands {
            [] => 1,
            [count] => match count.parse::<usize>() {
                Ok(count) => count,
                Err(_) => {
                    return failure(1, "shift", &format!("{count}: numeric argument required"));
                }
            },
            _ => return failure(1, "shift", "too many arguments"),
        };
        if count > self.positional.len() {
            return failure(1, "shift", "shift count out of range");
        }
        self.positional.drain(..count);
        status_only(0)
    }

    fn return_builtin(&mut self, operands: &[String], previous_status: i32) -> CommandOutcome {
        if self.call_depth == 0 {
            return failure(1, "return", "can only be used in a function");
        }
        let status = match operands {
            [] => previous_status,
            [value] => match value.parse::<i64>() {
                Ok(value) => i32::try_from(value.rem_euclid(256)).unwrap_or(1),
                Err(_) => {
                    return failure(2, "return", &format!("{value}: numeric argument required"));
                }
            },
            _ => return failure(2, "return", "too many arguments"),
        };
        self.pending_flow = Some(Flow::Return);
        status_only(status)
    }

    fn loop_control_builtin(&mut self, name: &str, operands: &[String]) -> CommandOutcome {
        let levels = match operands {
            [] => 1,
            [value] => match value.parse::<u32>() {
                Ok(levels) if levels > 0 => levels,
                _ => return failure(1, name, &format!("{value}: loop count out of range")),
            },
            _ => return failure(1, name, "too many arguments"),
        };
        if self.loop_depth == 0 {
            return failure(
                1,
                name,
                "only meaningful in a `for`, `while`, or `until` loop",
            );
        }
        let depth = u32::try_from(self.loop_depth).unwrap_or(u32::MAX);
        let levels = levels.min(depth);
        self.pending_flow = Some(if name == "break" {
            Flow::Break(levels)
        } else {
            Flow::Continue(levels)
        });
        status_only(0)
    }

    /// Run an expanded pipeline whose first stage is a function or an
    /// in-process built-in: that stage runs here with captured output, and
    /// its bytes become the next stage's standard input, so
    /// `myfunction | grep x` and `alias | sort` work without a subshell.
    ///
    /// Returns `None` when the first stage is an external program. The first
    /// stage's output is bounded by the request's capture limit, and it
    /// finishes before the rest of the pipeline starts, so this is not a
    /// streaming pipe. Later stages must be external programs.
    pub(super) fn execute_in_process_first_stage(
        &mut self,
        pipeline: &Pipeline,
        source: &str,
        capture: bool,
        request: Option<RequestContext<'_>>,
        observer: Option<&OutputObserverHandle<'_>>,
        previous_status: i32,
    ) -> Result<Option<CommandOutcome>, ShellError> {
        let Some((first, rest)) = pipeline.commands.split_first() else {
            return Ok(None);
        };
        let Some(name) = first.words.first() else {
            return Ok(None);
        };
        let function = self.functions.get(name).cloned();
        if function.is_none() && !NATIVE_BUILTINS.contains(&name.as_str()) {
            return Ok(None);
        }
        if let Some(later) = rest.iter().find_map(|command| {
            command
                .words
                .first()
                .filter(|name| self.functions.contains_key(*name))
        }) {
            return Err(ShellError::new(
                ErrorCode::InvalidCommand,
                format!("function `{later}` can only be the first stage of a native pipeline"),
            )
            .with_command(source)
            .with_help(
                "Move the function to the start of the pipeline, or run it in `bash { ... }`",
            ));
        }
        let produced = match function {
            Some(definition) => self.call_function(
                &definition,
                first,
                source,
                true,
                request,
                None,
                previous_status,
            )?,
            None => match self.execute_native_builtin(first, true, request, previous_status)? {
                Some(result) => result,
                None => return Ok(None),
            },
        };
        // A `return` or `break` inside the first stage ends only that stage,
        // as it would in the subshell other shells use for it.
        if self.pending_flow != Some(Flow::Abort) {
            self.pending_flow = None;
        }
        let stage_stderr = produced.stderr.unwrap_or_default();
        if !capture && !stage_stderr.is_empty() {
            super::io_write_all(std::io::stderr(), stage_stderr.as_bytes(), "standard error")?;
        }
        let remaining = Pipeline {
            commands: rest.to_vec(),
            background: pipeline.background,
        };
        self.pending_stdin = Some(produced.stdout.unwrap_or_default().into_bytes());
        let result = self.run_external_pipeline(&remaining, source, capture, request, observer);
        self.pending_stdin = None;
        let mut result = result?;
        if capture && !stage_stderr.is_empty() {
            let own = result.stderr.take().unwrap_or_default();
            result.stderr = Some(stage_stderr + &own);
        }
        Ok(Some(result))
    }

    /// Remove functions named by `unset -f`.
    pub(super) fn unset_functions(&mut self, names: &[String]) {
        for name in names {
            if self.functions.remove(name).is_some() {
                self.note_session_commands_changed();
            }
        }
    }
}

/// Deliver `echo` output that is not UTF-8, such as `echo -e '\xff'`.
///
/// Outcomes carry text, so bytes reach a `>` file or the terminal directly
/// and unchanged. Only captured output, which is displayed as text, falls
/// back to a lossy conversion.
fn write_raw_echo(
    command: &SimpleCommand,
    bytes: Vec<u8>,
    capture: bool,
    request: Option<RequestContext<'_>>,
) -> Result<CommandOutcome, ShellError> {
    let last_stdout = command.redirects.iter().rev().find(|redirect| {
        redirect.fd == 1
            && matches!(
                redirect.kind,
                RedirectKind::Output | RedirectKind::Append | RedirectKind::DuplicateOutput
            )
    });
    match last_stdout {
        Some(redirect) if redirect.kind != RedirectKind::DuplicateOutput => {
            let file = super::open_redirected_output(redirect, request)?;
            super::io_write_all(file, &bytes, &redirect.path)?;
            Ok(outcome(
                0,
                capture.then(String::new),
                capture.then(String::new),
            ))
        }
        None if !capture => {
            super::io_write_all(std::io::stdout(), &bytes, "standard output")?;
            Ok(outcome(0, None, None))
        }
        _ => finish_control_builtin(
            command,
            outcome(
                0,
                Some(String::from_utf8_lossy(&bytes).into_owned()),
                Some(String::new()),
            ),
            capture,
            request,
        ),
    }
}

/// A word that expands to exactly `text`.
fn literal_word(text: &str) -> Word {
    Word {
        parts: vec![WordPart {
            text: text.to_owned(),
            quoting: Quoting::Single,
        }],
    }
}

enum LoopStep {
    Continue,
    Exit(Flow),
}

/// Decide what a loop does after one list of its condition or body ran.
fn loop_step(flow: Flow) -> LoopStep {
    match flow {
        Flow::Next | Flow::Continue(1) => LoopStep::Continue,
        Flow::Break(1) => LoopStep::Exit(Flow::Next),
        Flow::Break(levels) => LoopStep::Exit(Flow::Break(levels.saturating_sub(1))),
        Flow::Continue(levels) => LoopStep::Exit(Flow::Continue(levels.saturating_sub(1))),
        Flow::Return | Flow::Abort => LoopStep::Exit(flow),
    }
}

fn status_only(status: i32) -> CommandOutcome {
    outcome(status, Some(String::new()), Some(String::new()))
}

fn failure(status: i32, name: &str, message: &str) -> CommandOutcome {
    outcome(
        status,
        Some(String::new()),
        Some(format!("{name}: {message}\n")),
    )
}

/// Every name `type` reports as a shell builtin.
pub(crate) fn is_shell_builtin(name: &str) -> bool {
    NATIVE_BUILTINS.contains(&name) || crate::builtin::is_stateful(name)
}

/// Whether `name` can be an alias: non-empty, without quoting, expansion,
/// `/`, `=`, or whitespace.
fn is_alias_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            !character.is_whitespace()
                && !matches!(
                    character,
                    '/' | '='
                        | '$'
                        | '`'
                        | '\''
                        | '"'
                        | '\\'
                        | '|'
                        | '&'
                        | ';'
                        | '<'
                        | '>'
                        | '('
                        | ')'
                )
        })
}

/// Quote `value` so `sh` reads it back unchanged.
fn single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Match `subject` against a `case` pattern whose characters carry whether
/// they are literal. Unquoted `*`, `?`, and `[...]` have their pattern
/// meaning and, unlike pathname expansion, also match `/` and a leading `.`.
///
/// Uses one row of booleans per pattern atom, so time is
/// O(pattern × subject) and memory O(subject).
fn case_pattern_matches(pattern: &[(char, bool)], subject: &str) -> bool {
    let subject: Vec<char> = subject.chars().collect();
    let mut previous = vec![false; subject.len().saturating_add(1)];
    if let Some(first) = previous.first_mut() {
        *first = true;
    }
    let mut index = 0_usize;
    while let Some(&(character, literal)) = pattern.get(index) {
        let mut current = vec![false; previous.len()];
        let mut advance = 1_usize;
        if !literal && character == '*' {
            let mut reachable = false;
            for (slot, &matched) in current.iter_mut().zip(previous.iter()) {
                reachable |= matched;
                *slot = reachable;
            }
        } else if !literal && character == '?' {
            for (slot, &matched) in current.iter_mut().skip(1).zip(previous.iter()) {
                *slot = matched;
            }
        } else if !literal
            && character == '['
            && let Some((class, consumed)) = bracket_class(pattern.get(index.saturating_add(1)..))
        {
            for ((slot, &matched), candidate) in current
                .iter_mut()
                .skip(1)
                .zip(previous.iter())
                .zip(&subject)
            {
                *slot = matched && class.matches(*candidate);
            }
            advance = consumed.saturating_add(1);
        } else {
            for ((slot, &matched), candidate) in current
                .iter_mut()
                .skip(1)
                .zip(previous.iter())
                .zip(&subject)
            {
                *slot = matched && *candidate == character;
            }
        }
        previous = current;
        index = index.saturating_add(advance);
    }
    previous.last().copied().unwrap_or(false)
}

struct BracketClass {
    negated: bool,
    ranges: Vec<(char, char)>,
}

impl BracketClass {
    fn matches(&self, character: char) -> bool {
        let contained = self
            .ranges
            .iter()
            .any(|(start, end)| *start <= character && character <= *end);
        contained != self.negated
    }
}

/// Parse a bracket expression after its `[`. Returns the class and how many
/// pattern characters it used, including the closing `]`, or `None` when the
/// bracket is unterminated and should match a literal `[`.
fn bracket_class(rest: Option<&[(char, bool)]>) -> Option<(BracketClass, usize)> {
    let rest = rest?;
    let mut negated = false;
    let mut position = 0_usize;
    if let Some((first, false)) = rest.first().copied()
        && matches!(first, '!' | '^')
    {
        negated = true;
        position = 1;
    }
    let content_start = position;
    let mut members = Vec::new();
    loop {
        let (character, literal) = rest.get(position).copied()?;
        // A `]` right after the opening bracket is a member, not the end.
        if character == ']' && !literal && position > content_start {
            break;
        }
        members.push(character);
        position = position.saturating_add(1);
    }
    let mut ranges = Vec::new();
    let mut member = 0_usize;
    while let Some(&start) = members.get(member) {
        if members.get(member.saturating_add(1)) == Some(&'-')
            && let Some(&end) = members.get(member.saturating_add(2))
        {
            ranges.push((start, end));
            member = member.saturating_add(3);
        } else {
            ranges.push((start, start));
            member = member.saturating_add(1);
        }
    }
    Some((BracketClass { negated, ranges }, position.saturating_add(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unquoted(pattern: &str) -> Vec<(char, bool)> {
        pattern
            .chars()
            .map(|character| (character, false))
            .collect()
    }

    #[test]
    fn case_patterns_match_like_sh() {
        for (pattern, subject, expected) in [
            ("*", "", true),
            ("*", "a/b", true),
            ("*.rs", ".hidden.rs", true),
            ("a?c", "abc", true),
            ("a?c", "ac", false),
            ("[abc]x", "bx", true),
            ("[!abc]x", "bx", false),
            ("[a-c]*", "cat", true),
            ("[]]", "]", true),
            ("[", "[", true),
            ("x[", "x[", true),
            ("lit", "lit", true),
            ("lit", "lite", false),
        ] {
            assert_eq!(
                case_pattern_matches(&unquoted(pattern), subject),
                expected,
                "{pattern} vs {subject}"
            );
        }
    }

    #[test]
    fn quoted_pattern_characters_match_only_themselves() {
        let pattern = vec![('*', true)];
        assert!(case_pattern_matches(&pattern, "*"));
        assert!(!case_pattern_matches(&pattern, "anything"));
        let mixed = vec![('a', false), ('*', true), ('*', false)];
        assert!(case_pattern_matches(&mixed, "a*xyz"));
        assert!(!case_pattern_matches(&mixed, "abc"));
    }

    #[test]
    fn loop_steps_unwind_nested_levels() {
        assert!(matches!(loop_step(Flow::Next), LoopStep::Continue));
        assert!(matches!(loop_step(Flow::Continue(1)), LoopStep::Continue));
        assert!(matches!(
            loop_step(Flow::Break(1)),
            LoopStep::Exit(Flow::Next)
        ));
        assert!(matches!(
            loop_step(Flow::Break(3)),
            LoopStep::Exit(Flow::Break(2))
        ));
        assert!(matches!(
            loop_step(Flow::Continue(2)),
            LoopStep::Exit(Flow::Continue(1))
        ));
        assert!(matches!(
            loop_step(Flow::Return),
            LoopStep::Exit(Flow::Return)
        ));
    }

    fn run(executor: &mut NativeExecutor, source: &str) -> (i32, String, String) {
        let result = executor
            .execute_capture(source)
            .unwrap_or_else(|error| panic!("{source:?} failed: {error:?}"));
        (
            result.status,
            result.stdout.unwrap_or_default(),
            result.stderr.unwrap_or_default(),
        )
    }

    fn stdout(source: &str) -> String {
        run(&mut NativeExecutor::default(), source).1
    }

    /// Programs whose output must equal `/bin/sh`'s, byte for byte. They
    /// avoid `echo` escapes, where POSIX shells legitimately differ.
    const DIFFERENTIAL_PROGRAMS: [&str; 24] = [
        "for x in a b c; do printf '%s,' \"$x\"; done",
        "for x in; do printf no; done; printf done",
        "i=0; while test $i -lt 3; do printf $i; i=$((i + 1)); done",
        "i=0; until test $i -ge 2; do printf u$i; i=$((i + 1)); done",
        "if false; then printf a; elif true; then printf b; else printf c; fi",
        "if false; then printf a; fi; printf \"[$?]\"",
        "case hello in h*) printf glob;; *) printf other;; esac",
        "case 'a b' in \"a b\") printf quoted;; esac",
        "case x in a|x) printf alt;; esac; case y in a) printf no;; esac; printf \"[$?]\"",
        "case '*' in \\*) printf literal-star;; esac",
        "f() { printf '%s|' \"$#\" \"$1\" \"$2\"; }; f one 'two words'",
        "f() { for a in \"$@\"; do printf '<%s>' \"$a\"; done; }; f a 'b c' ''",
        "f() { for a in $@; do printf '<%s>' \"$a\"; done; }; f a 'b c'",
        "f() { printf '<%s>' \"$*\"; }; f a b",
        "f() { return 3; }; f; printf \"[$?]\"",
        "f() { if test \"$1\" -gt 0; then printf $1; f $(($1 - 1)); fi; }; f 3",
        "for i in 1 2 3; do for j in a b; do test $j = b && continue 2; printf $i$j; done; done",
        "for i in 1 2 3; do test $i = 2 && break; printf $i; done; printf end",
        "x='a  b\tc'; for w in $x; do printf '<%s>' \"$w\"; done",
        "e=; printf '<%s>' $e x \"$e\"",
        "IFS=:; v=a:b::c; for w in $v; do printf '<%s>' \"$w\"; done",
        "{ printf a; printf b; }; ! false && printf c",
        "x=1; f() { x=2; }; f; printf $x",
        "set_it() { y=inner; }; set_it; printf \"$y\"",
    ];

    #[test]
    fn native_programs_match_the_reference_sh() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        for program in DIFFERENTIAL_PROGRAMS {
            let reference = std::process::Command::new("/bin/sh")
                .args(["-c", program])
                .output()
                .unwrap();
            let (status, native, _) = run(&mut NativeExecutor::default(), program);
            assert_eq!(
                native,
                String::from_utf8_lossy(&reference.stdout),
                "stdout differs for {program}"
            );
            assert_eq!(
                Some(status),
                reference.status.code(),
                "status differs for {program}"
            );
        }
    }

    #[test]
    fn functions_persist_across_commands_and_can_be_removed() {
        let mut executor = NativeExecutor::default();
        run(&mut executor, "greet() { echo \"hi $1\"; }");
        assert_eq!(run(&mut executor, "greet you").1, "hi you\n");
        assert_eq!(executor.function_names(), ["greet"]);
        let (status, listing, _) = run(&mut executor, "type greet");
        assert_eq!(status, 0);
        assert!(listing.starts_with("greet is a function"));
        run(&mut executor, "unset -f greet");
        assert!(executor.function_names().is_empty());
    }

    #[test]
    fn local_variables_are_restored_when_the_function_returns() {
        let mut executor = NativeExecutor::default();
        let program = "x=outer; export y=exported; \
            f() { local x=inner y; printf \"$x[${y-unset}]\"; y=changed; }; \
            f; printf \" $x $y\"";
        assert_eq!(run(&mut executor, program).1, "inner[unset] outer exported");
        assert!(
            executor
                .environment_snapshot()
                .unwrap()
                .iter()
                .any(|(name, value)| name == "y" && value == "exported")
        );
        let (status, _, stderr) = run(&mut executor, "local z=1");
        assert_eq!(status, 1);
        assert!(stderr.contains("only be used in a function"));
    }

    #[test]
    fn positional_parameters_are_scoped_to_each_call() {
        let program = "inner() { printf \"[$1]\"; }; \
            outer() { inner x; printf \"($1,$#)\"; shift; printf \"($1,$#)\"; }; \
            outer a b; printf \"{$#}\"";
        assert_eq!(stdout(program), "[x](a,2)(b,1){0}");
    }

    #[test]
    fn session_commands_generation_moves_only_when_function_or_alias_names_change() {
        let mut executor = NativeExecutor::default();
        let start = executor.session_commands_generation();
        run(&mut executor, "echo nothing-to-define");
        assert_eq!(executor.session_commands_generation(), start);
        run(&mut executor, "greet() { echo hi; }");
        let defined = executor.session_commands_generation();
        assert!(defined > start);
        run(&mut executor, "alias ll='ls -la'");
        let aliased = executor.session_commands_generation();
        assert!(aliased > defined);
        run(&mut executor, "unalias nope");
        assert_eq!(executor.session_commands_generation(), aliased);
        run(&mut executor, "unalias ll");
        let unaliased = executor.session_commands_generation();
        assert!(unaliased > aliased);
        run(&mut executor, "unset -f nope");
        assert_eq!(executor.session_commands_generation(), unaliased);
        run(&mut executor, "unset -f greet");
        assert!(executor.session_commands_generation() > unaliased);
        assert!(executor.function_names().is_empty());
    }

    #[test]
    fn aliases_expand_and_persist_and_list_in_sh_syntax() {
        let mut executor = NativeExecutor::default();
        run(&mut executor, "alias say='printf \"%s.\" ' hi=hello");
        assert_eq!(run(&mut executor, "say hi").1, "hello.");
        assert_eq!(
            run(&mut executor, "alias").1,
            "alias hi='hello'\nalias say='printf \"%s.\" '\n"
        );
        run(&mut executor, "alias printf='printf X'");
        assert_eq!(run(&mut executor, "printf %s").1, "X");
        let (status, _, stderr) = run(&mut executor, "unalias nope");
        assert_eq!(status, 1);
        assert!(stderr.contains("nope"));
        run(&mut executor, "unalias -a");
        assert!(executor.aliases().is_empty());
    }

    #[test]
    fn echo_and_test_run_in_process_with_bash_semantics() {
        assert_eq!(
            stdout("echo -n a; echo -e 'b\\tc'; echo -- d"),
            "ab\tc\n-- d\n"
        );
        assert_eq!(stdout("[ -d / ] && test -n x && printf ok"), "ok");
        let (status, _, stderr) = run(&mut NativeExecutor::default(), "[ 1 -eq");
        assert_eq!(status, 2);
        assert!(stderr.starts_with('['));
        assert_eq!(
            stdout("true; printf $?; false; printf $?; :; printf $?"),
            "010"
        );
    }

    #[test]
    fn loop_control_outside_a_loop_reports_and_fails() {
        let (status, _, stderr) = run(&mut NativeExecutor::default(), "break");
        assert_eq!(status, 1);
        assert!(stderr.contains("only meaningful"));
        let (status, _, stderr) = run(&mut NativeExecutor::default(), "return 1");
        assert_eq!(status, 1);
        assert!(stderr.contains("function"));
    }

    #[test]
    fn break_inside_a_function_does_not_escape_into_the_callers_loop() {
        let program = "f() { break; }; for i in 1 2; do f; printf $i; done";
        assert_eq!(stdout(program), "12");
    }

    #[test]
    fn runaway_recursion_hits_the_call_depth_limit() {
        let error = NativeExecutor::default()
            .execute_capture("f() { f; }; f")
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.message.contains("nest too deeply"));
    }

    #[test]
    fn the_interrupt_flag_stops_a_loop_that_spawns_nothing() {
        let mut executor = NativeExecutor::default();
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        executor.set_interrupt_flag(Some(Arc::clone(&flag)));
        let trigger = Arc::clone(&flag);
        let setter = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            trigger.store(true, Ordering::Relaxed);
        });
        let result = executor.execute_capture("while true; do :; done").unwrap();
        setter.join().unwrap();
        assert_eq!(result.status, INTERRUPTED_STATUS);
    }

    #[test]
    fn function_output_can_be_redirected_to_a_file() {
        let directory =
            std::env::temp_dir().join(format!("quirl-function-redirect-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("out.txt");
        let program = format!(
            "f() {{ printf one; printf two; }}; f > '{}'; printf done",
            path.display()
        );
        assert_eq!(stdout(&program), "done");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "onetwo");
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn unsupported_forms_report_why_and_how_to_run_them() {
        let error = NativeExecutor::default()
            .execute_capture("[[ -n x ]]")
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidCommand);
        assert!(error.message.contains("not native"));
        assert!(
            error
                .details
                .help
                .iter()
                .any(|help| help.contains("bash { ... }"))
        );
    }

    #[test]
    fn functions_and_builtins_can_feed_a_pipeline() {
        let mut executor = NativeExecutor::default();
        let program = "lines() { printf 'b\\na\\n'; }; lines | sort; echo hi | tr a-z A-Z; alias z=1 y=2; alias | wc -l | tr -d ' '";
        assert_eq!(run(&mut executor, program).1, "a\nb\nHI\n2\n");
        let error = executor.execute_capture("echo x | lines").unwrap_err();
        assert!(error.message.contains("first stage"));
    }

    #[test]
    fn redirected_compound_commands_stream_every_command_to_one_file() {
        let directory =
            std::env::temp_dir().join(format!("quirl-compound-redirect-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("out.txt");
        std::fs::write(&path, "stale\n").unwrap();
        let program = format!(
            "for i in 1 2; do echo line$i; printf 'err%s\\n' $i >&2; /bin/echo ext$i; done > '{0}' 2>&1; {{ echo more; }} >> '{0}'; printf done",
            path.display()
        );
        assert_eq!(stdout(&program), "done");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "line1\nerr1\next1\nline2\nerr2\next2\nmore\n"
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn stdout_duplicated_to_stderr_reaches_stderr_for_builtins_and_programs() {
        let (status, out, err) = run(
            &mut NativeExecutor::default(),
            "echo one >&2; /bin/echo two >&2; printf three 1>&2; die() { echo \"fatal: $1\" >&2; return 3; }; die four",
        );
        assert_eq!(status, 3);
        assert_eq!(out, "");
        assert_eq!(err, "one\ntwo\nthreefatal: four\n");
        let error = NativeExecutor::default()
            .execute_capture("/bin/echo x >&2 2>/dev/null")
            .unwrap_err();
        assert!(error.message.contains("`>&2`"));
        let path = std::env::temp_dir().join(format!("quirl-dup-override-{}", std::process::id()));
        let (_, _, err) = run(
            &mut NativeExecutor::default(),
            &format!("/bin/echo kept >&2 > '{}'", path.display()),
        );
        assert_eq!(err, "", "a later file redirection overrides `>&2`");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "kept\n");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn echo_with_non_utf8_output_keeps_its_raw_bytes() {
        let path = std::env::temp_dir().join(format!("quirl-echo-bytes-{}", std::process::id()));
        run(
            &mut NativeExecutor::default(),
            &format!("echo -ne 'A\\xffB' > '{}'", path.display()),
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"A\xffB");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn command_substitution_runs_functions_and_loops() {
        assert_eq!(
            stdout("f() { printf sub; }; printf \"[$(f)][$(for i in 1 2; do printf $i; done)]\""),
            "[sub][12]"
        );
    }

    #[test]
    fn alias_listings_quote_values_for_sh() {
        assert_eq!(single_quote("it's"), "'it'\\''s'");
        assert!(is_alias_name("ll"));
        assert!(is_alias_name("git-st"));
        assert!(!is_alias_name("a/b"));
        assert!(!is_alias_name("a b"));
        assert!(!is_alias_name(""));
    }
}
