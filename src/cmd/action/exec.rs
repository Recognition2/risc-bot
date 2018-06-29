extern crate htmlescape;
extern crate tokio_timer;

use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use futures::{
    future::{err, ok},
    Future,
    Stream,
};
use telegram_bot::{
    Api,
    prelude::*,
    types::{Message, MessageKind, ParseMode},
};
use self::htmlescape::encode_minimal;
use self::tokio_timer::Interval;

use executor::isolated;
use super::Action;

/// The action command name.
const CMD: &'static str = "exec";

/// The action help.
const HELP: &'static str = "Execute a shell command";

pub struct Exec;

impl Exec {
    pub fn new() -> Self {
        Exec
    }

    /// Execute the given command in isolated environment.
    ///
    /// Send a reply to the given user command `msg`
    /// and timely update it to show the status of the command that was executed.
    pub fn exec_cmd<'a>(cmd: String, api: Api, msg: &Message) -> Box<Future<Item = (), Error = ()>> {
        // Create the status message, and build the executable status object
        let exec_status = ExecStatus::create_status_msg(msg, api.clone());

        // Build a future for the command execution, and status updating
        let exec = exec_status.and_then(move |status| {
            // Create an mutexed arc for the status
            let status = Arc::new(Mutex::new(status));

            // Execute the command in an isolated environment, process output and the exit code
            let status_output = status.clone();
            let status_exit = status.clone();
            let cmd = isolated::execute(cmd, move |line| {
                    // Append the line to the captured output
                    status_output.lock().unwrap().append_line(&line);
                    Ok(())
                })
                .and_then(move |status| {
                    // Set the exit status
                    status_exit.lock().unwrap().set_status(status);
                    ok(())
                });

            // Set up an interval for constantly updating the status
            let status_update = status.clone();
            Interval::new(
                    Instant::now() + Duration::from_millis(1000),
                    Duration::from_millis(1000),
                )
                .for_each(move |_| {
                    // Update the status on Telegram, throttled
                    status_update.lock().unwrap().update_throttled();
                    Ok(())
                })
                .map_err(|_| ())
                .select(cmd)
                .and_then(move |_| {
                    // Update one final time, to ensure all status is sent to Telegram
                    status.lock().unwrap().update();
                    ok(())
                })
                .map_err(|_| ())
        });

        Box::new(exec)
    }
}

impl Action for Exec {
    fn cmd(&self) -> &'static str {
        CMD
    }

    fn help(&self) -> &'static str {
        HELP
    }

    // TODO: proper error handling everywhere, pass errors along
    fn invoke(&self, msg: &Message, api: &Api) -> Box<Future<Item = (), Error = ()>> {
        if let MessageKind::Text {
            ref data,
            ..
        } = &msg.kind {

            // The Telegram API client to use
            let api = api.clone();

            // The command to run in the shell
            // TODO: actually properly fetch the command to execute from the full message
            let cmd = data.splitn(2, ' ')
                .skip(1)
                .next()
                .map(|cmd| cmd.trim_left())
                .unwrap_or("")
                .to_owned();

            // Provide the user with feedback if no command is entered
            if cmd.trim().is_empty() {
                api.spawn(
                    msg.text_reply("\
                        Please provide a command to run.\n\
                        \n\
                        For example:\n\
                        `/exec echo Hello!`\
                    ").parse_mode(ParseMode::Markdown),
                );
                return Box::new(ok(()));
            }

            // Print the command to run
            println!("CMD: {}", cmd);

            // Execute the command, report back to the user
            return Self::exec_cmd(cmd, api, msg);
        }

        Box::new(ok(()))
    }
}

/// An object that tracks the status of an executed command.
/// This object also holds the status message present in a Telegram group to update when the status
/// changes, along with an Telegram API instance.
// TODO: detect timeouts
// TODO: report execution times
pub struct ExecStatus {
    /// The actual output.
    output: String,

    /// The exit status of the process.
    /// If set, the execution has completed although it might have failed.
    /// The status code itself defines whether the execution was successful.
    status: Option<ExitStatus>,

    /// True if the output or status has changed since the last status message update.
    /// If true, this means that the status message doesn't represent the current status corretly,
    /// and thus it should be updated.
    changed: bool,

    /// The time the Telegram status message was last changed at.
    /// When the status instance is created, this is set to the current time.
    /// This is used to manage throttling.
    changed_at: SystemTime,

    /// An Telegram API client to update the status message with.
    api: Api,

    /// The status message in a Telegram chat that should be updated to report the executing
    /// status.
    status_msg: Message,
}

impl ExecStatus {
    /// Create a status output message as reply on the given `msg`,
    /// and return an `ExecStatus` for it.
    pub fn create_status_msg(msg: &Message, api: Api)
        -> impl Future<Item = Self, Error = ()>
    {
        // TODO: make this timeout configurable
        // TODO: handle the Telegram errors properly
        api.send_timeout(
                msg.text_reply("<i>Executing command...</i>")
                    .parse_mode(ParseMode::Html),
                Duration::from_secs(10),
            )
            .map_err(|err| println!("TELEGRAM ERROR: {:?}", err))
            .and_then(|msg| if let Some(msg) = msg {
                ok(ExecStatus::new(msg, api))
            } else {
                err(())
            })
            .map_err(|_| println!("TELEGRAM ERROR: no message"))
    }

    /// Build a new exec status object with the given status message and Telegram API client
    /// instance.
    pub fn new(status_msg: Message, api: Api) -> Self {
        ExecStatus {
            output: String::new(),
            status: None,
            changed: false,
            changed_at: SystemTime::now(),
            api,
            status_msg,
        }
    }

    /// Append the given `output` to the cummulative output.
    /// Note that if the output is getting too large, parts will be truncated.
    /// To append a line, use `append_line()` instead.
    pub fn append(&mut self, output: &str) {
        let truncate_at = 4096 - 100;

        // Do not append if the output became too large
        if self.output.len() > truncate_at {
            return;
        }

        // Append the output
        self.output += output;

        // Truncate the output if it became too big
        if self.output.len() >= truncate_at {
            self.output.truncate(truncate_at);
            self.output += " [truncated]";
        }

        // If anything is appended, we've changed
        if !output.is_empty() {
            self.changed = true;
        }
    }

    /// Append the given `line` to the output.
    /// The given line should not include a newline character.
    /// Note that if the output is getting too large, parts will be truncated.
    pub fn append_line(&mut self, line: &str) {
        if !self.output.is_empty() {
            self.append("\n");
        }
        self.append(line);
    }

    /// Set the exit status of the executed command.
    pub fn set_status(&mut self, status: ExitStatus) {
        // Mark that the status has changed if the exit status is different
        if self.status != Some(status) {
            self.changed = true;
        }

        // Update the status
        self.status = Some(status);
    }

    /// Check whether this executable has completed.
    /// It may have successfully completed or it may have failed.
    pub fn completed(&self) -> bool {
        self.status.is_some()
    }

    /// Build the status message contents, based on the current executing status.
    /// The returned status message is in HTML format.
    fn build_status_msg(&self) -> String {
        // If not completed, and there is no output yet
        if !self.completed() && self.output.is_empty() {
            return "<i>Executing command...</i>".into();
        }

        // Determine what status emoji to use
        let emoji = if !self.completed() {
            "⏳"
        } else if self.status.unwrap().success() {
            "✅"
        } else {
            "❌"
        };

        // Deterime whether to print a special notice
        let notice = match self.status {
            Some(status) if !status.success() =>
                format!(
                    "   Exit code <code>{}</code>",
                    status.code()
                        .map(|code| code.to_string())
                        .unwrap_or("?".into()),
                ),
            _ => String::new(),
        };

        // Format the output
        let output = if self.output.is_empty() {
            "<i>No output</i>".to_owned()
        } else {
            format!("\
                    <b>Output:</b>\n\
                    <code>{}</code>\
                ",
                encode_minimal(&self.output),
            )
        };

        // Format the message
        format!("\
                {}\n\
                \n\
                {}{}\
            ",
            output,
            emoji,
            notice,
        )
    }

    /// Update the status message in Telegram with the newest status data in this object.
    /// This method spawns a future that completes asynchronously.
    pub fn update_status_msg(&mut self) {
        // Spawn a future to edit the status message with the newest build status text
        self.api.spawn(
            self.status_msg
                .edit_text(self.build_status_msg())
                .parse_mode(ParseMode::Html)
        );

        // Reset the changed status
        self.changed = false;
        self.changed_at = SystemTime::now();
    }

    /// Update the status message in Telegram with the newest status data in this object,
    /// if it has been changed after the last update.
    pub fn update(&mut self) {
        // Only if something changed
        if !self.changed {
            return;
        }

        // Actually update
        self.update_status_msg()
    }

    /// Update the status message in Telegram with the newest status data in this object,
    /// if it has been changed after the last update.
    ///
    /// This method won't update if it was invoked too quickly before the last change.
    pub fn update_throttled(&mut self) {
        // Throttle
        // TODO: make the throttle time configurable
        match self.changed_at.elapsed() {
            Ok(elapsed) if elapsed < Duration::from_millis(900) => return,
            Err(..) => return,
            _ => {},
        }

        // Update
        self.update()
    }
}