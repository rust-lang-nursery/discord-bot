#[macro_use]
extern crate diesel;

#[macro_use]
extern crate diesel_migrations;

#[macro_use]
extern crate log;

mod api;
mod ban;
mod commands;
mod crates;
mod db;
mod playground;
mod schema;
mod state_machine;
mod tags;
mod text;
mod welcome;

use crate::db::DB;
use commands::{Args, Commands, GuardFn, Result};
use diesel::prelude::*;
use envy;
use indexmap::IndexMap;
use serde::Deserialize;
use serenity::{model::prelude::*, prelude::*, utils::CustomMessage};
use std::collections::HashMap;

#[derive(Deserialize)]
struct Config {
    tags: bool,
    crates: bool,
    eval: bool,
    discord_token: String,
    mod_id: String,
    talk_id: String,
    wg_and_teams_id: Option<String>,
}

fn init_data(config: &Config) -> Result<()> {
    use crate::schema::roles;
    info!("Loading data into database");

    let conn = DB.get()?;

    let upsert_role = |name: &str, role_id: &str| -> Result<()> {
        diesel::insert_into(roles::table)
            .values((roles::role.eq(role_id), roles::name.eq(name)))
            .on_conflict(roles::name)
            .do_update()
            .set(roles::role.eq(role_id))
            .execute(&conn)?;

        Ok(())
    };

    let _ = conn
        .build_transaction()
        .read_write()
        .run::<_, Box<dyn std::error::Error>, _>(|| {
            upsert_role("mod", &config.mod_id)?;
            upsert_role("talk", &config.talk_id)?;

            if config.tags || config.crates {
                let wg_and_teams_role = config
                    .wg_and_teams_id
                    .as_ref()
                    .ok_or_else(|| "missing value for field wg_and_teams_id.\n\nIf you enabled tags or crates then you need the WG_AND_TEAMS_ID env var.")?;
                upsert_role("wg_and_teams", &wg_and_teams_role)?;
            }

            Ok(())
        })?;

    Ok(())
}

fn app() -> Result<()> {
    let config = envy::from_env::<Config>()?;

    info!("starting...");

    let _ = db::run_migrations()?;

    let _ = init_data(&config)?;

    let mut cmds = Commands::new();

    if config.tags {
        // Tags
        cmds.add_protected("?tags delete {key}", tags::delete, api::is_wg_and_teams);
        cmds.add_protected(
            "?tags create {key} value...",
            tags::post,
            api::is_wg_and_teams,
        );
        cmds.add("?tag {key}", tags::get);
        cmds.add("?tags", tags::get_all);
        cmds.help("?tags", "A key value store", tags::help);
    }

    if config.crates {
        // crates.io
        cmds.add("?crate query...", crates::search);
        cmds.help("?crate", "Lookup crates on crates.io", crates::help);

        // docs.rs
        cmds.add("?docs query...", crates::doc_search);
        cmds.help("?docs", "Lookup documentation", crates::doc_help);
    }

    if config.eval {
        // rust playground
        cmds.add(
            "?play mode={} edition={} channel={} warn={} ```\ncode```",
            playground::run,
        );
        cmds.add("?play code...", playground::err);
        cmds.help(
            "?play",
            "Compile and run rust code in a playground",
            |args| playground::help(args, "play"),
        );

        cmds.add(
            "?eval mode={} edition={} channel={} warn={} ```\ncode```",
            playground::eval,
        );
        cmds.add(
            "?eval mode={} edition={} channel={} warn={} ```code```",
            playground::eval,
        );
        cmds.add(
            "?eval mode={} edition={} channel={} warn={} `code`",
            playground::eval,
        );
        cmds.add("?eval code...", playground::eval_err);
        cmds.help("?eval", "Evaluate a single rust expression", |args| {
            playground::help(args, "eval")
        });
    }

    // Slow mode.
    // 0 seconds disables slowmode
    cmds.add_protected("?slowmode {channel} {seconds}", api::slow_mode, api::is_mod);
    cmds.help_protected(
        "?slowmode",
        "Set slowmode on a channel",
        api::slow_mode_help,
        api::is_mod,
    );

    // Kick
    cmds.add_protected("?kick {user}", api::kick, api::is_mod);
    cmds.help_protected(
        "?kick",
        "Kick a user from the guild",
        api::kick_help,
        api::is_mod,
    );

    // Ban
    cmds.add_protected("?ban {user} {hours} reason...", ban::temp_ban, api::is_mod);
    cmds.help_protected(
        "?ban",
        "Temporarily ban a user from the guild",
        ban::help,
        api::is_mod,
    );

    // Post the welcome message to the welcome channel.
    cmds.add_protected("?CoC {channel}", welcome::post_message, api::is_mod);
    cmds.help_protected(
        "?CoC",
        "Post the code of conduct message to a channel",
        welcome::help,
        api::is_mod,
    );

    let menu = cmds.menu();
    cmds.add("?help", move |args: Args| {
        let output = main_menu(&args, menu.as_ref().unwrap());
        api::send_reply(&args, &format!("```{}```", &output))?;
        Ok(())
    });

    let mut client = Client::new_with_extras(&config.discord_token, |e| {
        e.raw_event_handler(Events { cmds });
        e
    })?;

    client.start()?;

    Ok(())
}

fn main_menu(args: &Args, commands: &IndexMap<&str, (&str, GuardFn)>) -> String {
    let mut menu = format!("Commands:\n");

    menu = commands
        .iter()
        .fold(menu, |mut menu, (base_cmd, (description, guard))| {
            if let Ok(true) = (guard)(&args) {
                menu += &format!("\t{cmd:<12}{desc}\n", cmd = base_cmd, desc = description);
            }
            menu
        });

    menu += &format!("\t{help:<12}This menu\n", help = "?help");
    menu += "\nType ?help command for more info on a command.";
    menu
}

fn main() {
    env_logger::init();

    if let Err(e) = app() {
        error!("{}", e);
        std::process::exit(1);
    }
}

struct CommandHistory {}
impl TypeMapKey for CommandHistory {
    type Value = HashMap<MessageId, MessageId>;
}

struct Events {
    cmds: Commands,
}

impl RawEventHandler for Events {
    fn raw_event(&self, cx: Context, event: Event) {
        match event {
            Event::Ready(mut ev) => {
                info!("{} connected to discord", ev.ready.user.name);

                let mut cache = cx.cache.write();
                cache.update(&mut ev);
                drop(cache);

                let mut data = cx.data.write();
                data.insert::<CommandHistory>(HashMap::new());
                drop(data);

                ban::start_unban_thread(cx);
            }
            Event::MessageCreate(ev) => {
                self.cmds.execute(cx, &ev.message);
            }
            Event::MessageUpdate(ev) => {
                let mut msg = CustomMessage::new();

                msg.id(ev.id)
                    .channel_id(ev.channel_id)
                    .content(ev.content.unwrap_or_else(|| String::new()));

                let msg = msg.build();
                info!("sending edited message - {:?}", msg.content);
                self.cmds.execute(cx, &msg);
            }
            Event::MessageDelete(ev) => {
                let mut data = cx.data.write();
                let history = data.get_mut::<CommandHistory>().unwrap();
                if let Some(response_id) = history.remove(&ev.message_id) {
                    info!("deleting message: {:?}", response_id);
                    let _ = ev.channel_id.delete_message(&cx, response_id);
                }
            }
            Event::ReactionAdd(ev) => {
                if let Err(e) = welcome::assign_talk_role(&cx, &ev) {
                    error!("{}", e);
                }
            }
            Event::GuildBanRemove(ev) => {
                if let Err(e) =
                    ban::save_unban(format!("{}", ev.user.id), format!("{}", ev.guild_id))
                {
                    error!("{}", e);
                }
            }
            _ => (),
        }
    }
}
