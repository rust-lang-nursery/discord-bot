use crate::{
    api,
    state_machine::{CharacterSet, StateMachine},
    Error,
};
use indexmap::IndexMap;
use reqwest::blocking::Client as HttpClient;
use serenity::{model::channel::Message, prelude::Context};
use std::{collections::HashMap, sync::Arc};

pub(crate) const PREFIX: &str = "?";
pub(crate) type GuardFn = fn(&Args) -> Result<bool, Error>;

struct Command {
    guard: GuardFn,
    ptr: Box<dyn for<'m> Fn(Args<'m>) -> Result<(), Error> + Send + Sync>,
}

impl Command {
    fn authorize(&self, args: &Args) -> Result<bool, Error> {
        (self.guard)(&args)
    }

    fn call(&self, args: Args) -> Result<(), Error> {
        (self.ptr)(args)
    }
}

pub struct Args<'m> {
    pub http: &'m HttpClient,
    pub cx: &'m Context,
    pub msg: &'m Message,
    pub params: HashMap<&'m str, &'m str>,
}

pub(crate) struct Commands {
    state_machine: StateMachine<Arc<Command>>,
    client: HttpClient,
    menu: Option<IndexMap<&'static str, (&'static str, GuardFn)>>,
}

impl Commands {
    pub(crate) fn new() -> Self {
        Self {
            state_machine: StateMachine::new(),
            client: HttpClient::new(),
            menu: Some(IndexMap::new()),
        }
    }

    pub(crate) fn add(
        &mut self,
        command: &'static str,
        handler: impl Fn(Args) -> Result<(), Error> + Send + Sync + 'static,
    ) {
        self.add_protected(command, handler, |_| Ok(true));
    }

    pub(crate) fn add_protected(
        &mut self,
        command: &'static str,
        handler: impl Fn(Args) -> Result<(), Error> + Send + Sync + 'static,
        guard: GuardFn,
    ) {
        info!("Adding command {}", &command);
        let mut state = 0;

        let mut reused_space_state = None;
        let mut opt_final_states = vec![];

        let handler = Arc::new(Command {
            guard,
            ptr: Box::new(handler),
        });

        command
            .split(' ')
            .filter(|segment| segment.len() > 0)
            .enumerate()
            .for_each(|(i, segment)| {
                if let Some(name) = key_value_pair(segment) {
                    if let Some(lambda) = reused_space_state {
                        state = self.add_key_value(name, lambda);
                        self.state_machine.add_next_state(state, lambda);
                        opt_final_states.push(state);

                        state = self.add_quoted_key_value(name, lambda);
                        self.state_machine.add_next_state(state, lambda);
                        opt_final_states.push(state);
                    } else {
                        opt_final_states.push(state);
                        state = self.add_space(state, i);
                        reused_space_state = Some(state);

                        state = self.add_key_value(name, state);
                        self.state_machine
                            .add_next_state(state, reused_space_state.unwrap());
                        opt_final_states.push(state);

                        state = self.add_quoted_key_value(name, reused_space_state.unwrap());
                        self.state_machine
                            .add_next_state(state, reused_space_state.unwrap());
                        opt_final_states.push(state);
                    }
                } else {
                    reused_space_state = None;
                    opt_final_states.truncate(0);
                    let last_state = state;
                    state = self.add_space(state, i);

                    if segment.starts_with("```\n") && segment.ends_with("```") {
                        state = self.add_code_segment_multi_line(state, segment);
                    } else if segment.starts_with("```") && segment.ends_with("```") {
                        state = self.add_code_segment_single_line(state, segment, 3);
                    } else if segment.starts_with('`') && segment.ends_with('`') {
                        state = self.add_code_segment_single_line(state, segment, 1);
                    } else if segment.starts_with('{') && segment.ends_with('}') {
                        state = self.add_dynamic_segment(state, segment);
                    } else if segment.ends_with("...") {
                        if segment == "..." {
                            self.state_machine.set_final_state(last_state);
                            self.state_machine.set_handler(last_state, handler.clone());
                            state = self.add_unnamed_remaining_segment(last_state);
                        } else {
                            state = self.add_remaining_segment(state, segment);
                        }
                    } else {
                        segment.chars().for_each(|ch| {
                            state = self.state_machine.add(state, CharacterSet::from_char(ch))
                        });
                    }
                }
            });

        if reused_space_state.is_some() {
            opt_final_states.iter().for_each(|state| {
                self.state_machine.set_final_state(*state);
                self.state_machine.set_handler(*state, handler.clone());
            });
        } else {
            self.state_machine.set_final_state(state);
            self.state_machine.set_handler(state, handler);
        }
    }

    pub(crate) fn help(
        &mut self,
        cmd: &'static str,
        desc: &'static str,
        handler: impl Fn(Args) -> Result<(), Error> + Send + Sync + 'static,
    ) {
        self.help_protected(cmd, desc, handler, |_| Ok(true));
    }

    pub(crate) fn help_protected(
        &mut self,
        cmd: &'static str,
        desc: &'static str,
        handler: impl Fn(Args) -> Result<(), Error> + Send + Sync + 'static,
        guard: GuardFn,
    ) {
        let base_cmd = &cmd[1..];
        info!("Adding command ?help {}", &base_cmd);
        let mut state = 0;

        self.menu.as_mut().map(|menu| {
            menu.insert(cmd, (desc, guard));
            menu
        });

        state = self.add_help_menu(base_cmd, state);
        self.state_machine.set_final_state(state);
        self.state_machine.set_handler(
            state,
            Arc::new(Command {
                guard,
                ptr: Box::new(handler),
            }),
        );
    }

    pub(crate) fn menu(&mut self) -> Option<IndexMap<&'static str, (&'static str, GuardFn)>> {
        self.menu.take()
    }

    pub(crate) fn execute(&self, cx: Context, msg: &Message) {
        let message = &msg.content;
        if !msg.is_own(&cx) && message.starts_with(PREFIX) {
            self.state_machine.process(message).map(|matched| {
                info!("Processing command: {}", message);
                let args = Args {
                    http: &self.client,
                    cx: &cx,
                    msg: &msg,
                    params: matched.params,
                };
                info!("Checking permissions");
                match matched.handler.authorize(&args) {
                    Ok(true) => {
                        info!("Executing command");
                        if let Err(e) = matched.handler.call(args) {
                            error!("{}", e);
                        }
                    }
                    Ok(false) => {
                        info!("Not executing command, unauthorized");
                        if let Err(e) =
                            api::send_reply(&args, "You do not have permission to run this command")
                        {
                            error!("{}", e);
                        }
                    }
                    Err(e) => error!("{}", e),
                }
            });
        }
    }

    fn add_space(&mut self, mut state: usize, i: usize) -> usize {
        if i > 0 {
            let char_set = CharacterSet::from_chars(&[' ', '\n']);

            state = self.state_machine.add(state, char_set);
            self.state_machine.add_next_state(state, state);
        }
        state
    }

    fn add_help_menu(&mut self, cmd: &'static str, mut state: usize) -> usize {
        "?help".chars().for_each(|ch| {
            state = self.state_machine.add(state, CharacterSet::from_char(ch));
        });
        state = self.add_space(state, 1);
        cmd.chars().for_each(|ch| {
            state = self.state_machine.add(state, CharacterSet::from_char(ch));
        });

        state
    }

    fn add_dynamic_segment(&mut self, mut state: usize, s: &'static str) -> usize {
        let name = &s[1..s.len() - 1];

        let mut char_set = CharacterSet::any();
        char_set.remove(&[' ']);
        state = self.state_machine.add(state, char_set);
        self.state_machine.add_next_state(state, state);
        self.state_machine.start_parse(state, name);
        self.state_machine.end_parse(state);

        state
    }

    fn add_remaining_segment(&mut self, mut state: usize, s: &'static str) -> usize {
        let name = &s[..s.len() - 3];

        let char_set = CharacterSet::any();
        state = self.state_machine.add(state, char_set);
        self.state_machine.add_next_state(state, state);
        self.state_machine.start_parse(state, name);
        self.state_machine.end_parse(state);

        state
    }

    fn add_unnamed_remaining_segment(&mut self, mut state: usize) -> usize {
        let char_set = CharacterSet::any();
        state = self.state_machine.add(state, char_set);
        self.state_machine.add_next_state(state, state);

        state
    }

    fn add_code_segment_multi_line(&mut self, mut state: usize, s: &'static str) -> usize {
        let name = &s[4..s.len() - 3];

        "```".chars().for_each(|ch| {
            state = self.state_machine.add(state, CharacterSet::from_char(ch));
        });

        let lambda = state;

        let mut char_set = CharacterSet::any();
        char_set.remove(&['`', ' ', '\n']);
        state = self.state_machine.add(state, char_set);
        self.state_machine.add_next_state(state, state);

        state = self.state_machine.add(state, CharacterSet::from_char('\n'));

        self.state_machine.add_next_state(lambda, state);

        state = self.state_machine.add(state, CharacterSet::any());
        self.state_machine.add_next_state(state, state);
        self.state_machine.start_parse(state, name);
        self.state_machine.end_parse(state);

        "```".chars().for_each(|ch| {
            state = self.state_machine.add(state, CharacterSet::from_char(ch));
        });

        state
    }

    fn add_code_segment_single_line(
        &mut self,
        mut state: usize,
        s: &'static str,
        n_backticks: usize,
    ) -> usize {
        use std::iter::repeat;

        let name = &s[n_backticks..s.len() - n_backticks];

        repeat('`').take(n_backticks).for_each(|ch| {
            state = self.state_machine.add(state, CharacterSet::from_char(ch));
        });
        state = self.state_machine.add(state, CharacterSet::any());
        self.state_machine.add_next_state(state, state);
        self.state_machine.start_parse(state, name);
        self.state_machine.end_parse(state);
        repeat('`').take(n_backticks).for_each(|ch| {
            state = self.state_machine.add(state, CharacterSet::from_char(ch));
        });

        state
    }

    fn add_key_value(&mut self, name: &'static str, mut state: usize) -> usize {
        name.chars().for_each(|c| {
            state = self.state_machine.add(state, CharacterSet::from_char(c));
        });
        state = self.state_machine.add(state, CharacterSet::from_char('='));

        let mut char_set = CharacterSet::any();
        char_set.remove(&[' ', '\n', '"']);
        state = self.state_machine.add(state, char_set);
        self.state_machine.add_next_state(state, state);
        self.state_machine.start_parse(state, name);
        self.state_machine.end_parse(state);

        state
    }

    fn add_quoted_key_value(&mut self, name: &'static str, mut state: usize) -> usize {
        name.chars().for_each(|c| {
            state = self.state_machine.add(state, CharacterSet::from_char(c));
        });
        state = self.state_machine.add(state, CharacterSet::from_char('='));
        state = self.state_machine.add(state, CharacterSet::from_char('"'));

        let mut char_set = CharacterSet::any();
        char_set.remove(&['"']);
        state = self.state_machine.add(state, char_set);
        self.state_machine.add_next_state(state, state);
        self.state_machine.start_parse(state, name);
        self.state_machine.end_parse(state);

        state = self.state_machine.add(state, CharacterSet::from_char('"'));

        state
    }
}

fn key_value_pair(s: &'static str) -> Option<&'static str> {
    s.match_indices("={}")
        .next()
        .map(|pair| {
            let name = &s[0..pair.0];
            if name.len() > 0 {
                Some(name)
            } else {
                None
            }
        })
        .flatten()
}

mod test {
    use super::*;

    #[test]
    fn existing_commands_are_parsed_as_expected() {
        macro_rules! params {
            ($(($key:literal, $value:literal)),+) => (
                [$(($key, $value)),+].iter().fold(
                        HashMap::new(),
                        |mut hash_map, pair| {
                            hash_map.insert(pair.0, pair.1);
                            hash_map
                        },
                )
            );
        }

        let mut cmds = Commands::new();

        cmds.add("?tags delete {key}", |_: Args| Ok(()));
        cmds.add("?tags create {key} value...", |_: Args| Ok(()));
        cmds.add("?tags update {key} value...", |_: Args| Ok(()));
        cmds.add("?tag {key}", |_: Args| Ok(()));
        cmds.add("?tags", |_: Args| Ok(()));

        cmds.add("?crate query...", |_: Args| Ok(()));

        // tags

        let tags_delete = cmds.state_machine.process("?tags delete foo");
        assert!(tags_delete.is_some());
        tags_delete.map(|matched| assert!(params!(("key", "foo")) == matched.params));

        let tags_create = cmds.state_machine.process("?tags create foo foo bar baz");
        assert!(tags_create.is_some());
        tags_create.map(|matched| {
            assert!(params!(("key", "foo"), ("value", "foo bar baz")) == matched.params);
        });

        let tags_update = cmds.state_machine.process("?tags update foo 123 456 abc");
        assert!(tags_update.is_some());
        tags_update.map(|matched| {
            assert!(params!(("key", "foo"), ("value", "123 456 abc")) == matched.params);
        });

        let tag = cmds.state_machine.process("?tag foo");
        assert!(tag.is_some());
        tag.map(|matched| {
            assert!(params!(("key", "foo")) == matched.params);
        });

        assert!(cmds.state_machine.process("?tags").is_some());

        // crates

        let krate = cmds.state_machine.process("?crate 12345abc");
        assert!(krate.is_some());
        krate.map(|matched| {
            assert!(params!(("query", "12345abc")) == matched.params);
        });
    }
}
