use crate::{
    api,
    commands::Args,
    db::DB,
    schema::{messages, roles, users},
    text::WELCOME_BILLBOARD,
    Error,
};
use diesel::prelude::*;
use serenity::{model::prelude::*, prelude::*};

/// Write the welcome message to the welcome channel.  
pub(crate) fn post_message(args: Args) -> Result<(), Error> {
    use std::str::FromStr;

    if api::is_mod(&args)? {
        let channel_name = &args
            .params
            .get("channel")
            .ok_or("unable to retrieve channel param")?;

        let channel_id = ChannelId::from_str(channel_name)?;
        info!("Posting welcome message");
        let message = channel_id.say(&args.cx, WELCOME_BILLBOARD)?;
        let bot_id = &message.author.id;

        let conn = DB.get()?;

        let _ = conn
            .build_transaction()
            .read_write()
            .run::<_, Box<dyn std::error::Error>, _>(|| {
                let message_id = message.id.0.to_string();
                let channel_id = channel_id.0.to_string();

                diesel::insert_into(messages::table)
                    .values((
                        messages::name.eq("welcome"),
                        messages::message.eq(&message_id),
                        messages::channel.eq(&channel_id),
                    ))
                    .on_conflict(messages::name)
                    .do_update()
                    .set((
                        messages::message.eq(&message_id),
                        messages::channel.eq(&channel_id),
                    ))
                    .execute(&conn)?;

                let user_id = &bot_id.to_string();

                diesel::insert_into(users::table)
                    .values((users::user_id.eq(user_id), users::name.eq("me")))
                    .on_conflict(users::name)
                    .do_update()
                    .set((users::name.eq("me"), users::user_id.eq(user_id)))
                    .execute(&conn)?;
                Ok(())
            })?;

        let white_check_mark = ReactionType::from("✅");
        message.react(args.cx, white_check_mark)?;
    }
    Ok(())
}

pub(crate) fn assign_talk_role(cx: &Context, reaction: &Reaction) -> Result<(), Error> {
    let channel = reaction.channel(cx)?;
    let channel_id = ChannelId::from(&channel);
    let message = reaction.message(cx)?;

    let conn = DB.get()?;

    let (msg, talk_role, me) = conn
        .build_transaction()
        .read_only()
        .run::<_, Box<dyn std::error::Error>, _>(|| {
            let msg: Option<_> = messages::table
                .filter(messages::name.eq("welcome"))
                .first::<(i32, String, String, String)>(&conn)
                .optional()?;

            let role: Option<_> = roles::table
                .filter(roles::name.eq("talk"))
                .first::<(i32, String, String)>(&conn)
                .optional()?;

            let me: Option<_> = users::table
                .filter(users::name.eq("me"))
                .first::<(i32, String, String)>(&conn)
                .optional()?;

            Ok((msg, role, me))
        })?;

    if let Some((_, _, cached_message_id, cached_channel_id)) = msg {
        if message.id.0.to_string() == cached_message_id
            && channel_id.0.to_string() == *cached_channel_id
        {
            if reaction.emoji == ReactionType::from("✅") {
                if let Some((_, role_id, _)) = talk_role {
                    let user_id = reaction.user_id;

                    let guild = channel
                        .guild()
                        .ok_or("Unable to retrieve guild from channel")?;

                    let mut member = guild.read().guild_id.member(cx, &user_id)?;

                    use std::str::FromStr;
                    info!("Assigning talk role to {}", &member.user_id());
                    member.add_role(&cx, RoleId::from(u64::from_str(&role_id)?))?;

                    // Requires ManageMessage permission
                    if let Some((_, _, user_id)) = me {
                        if reaction.user_id.0.to_string() != user_id {
                            reaction.delete(cx)?;
                        }
                    }
                }
            } else {
                reaction.delete(cx)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn help(args: Args) -> Result<(), Error> {
    let help_string = format!(
        "
Post the welcome message to `channel`
```
{command}
```
**Example:**
```
?CoC #welcome

```
will post the welcome message to the `channel` specified.  
",
        command = "?CoC {channel}"
    );

    api::send_reply(&args, &help_string)?;
    Ok(())
}
