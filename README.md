# ccchat

**Text your questions. Get AI answers. In Signal.**

No apps to open. No browser tabs. Just send a message and Claude AI texts you back.

---

## What You Can Do

Once set up, you just text your ccchat number like any other contact:

> **You:** What's a good recipe for lemon chicken?
>
> **Claude:** Here's a simple lemon chicken that takes about 30 minutes...

> **You:** Summarise this email for me: [paste long email]
>
> **Claude:** Sure! The key points are...

> **You:** /remind 1h Take the chicken out of the oven
>
> **Claude:** ✓ Reminder set for 1 hour from now.

That's it. Everything lives in Signal. No new apps.

---

## What You Need

1. A computer running 24/7 (or a server) — this is where ccchat runs
2. A Signal account registered on that computer using [signal-cli](https://github.com/AsamK/signal-cli)
3. [Claude Code](https://claude.ai/download) installed and logged in on that computer

> **Not technical?** The setup takes about 15 minutes if you follow the steps in the [User Manual](USER_MANUAL.md). Once it's running, you never need to touch the computer again.

---

## Quick Setup

```bash
# 1. Install
cargo install ccchat

# 2. Run (replace with your Signal number)
ccchat --account +447700000000
```

Then text yourself from your phone. You're in.

---

## Invite a Friend

By default only you can chat. To let someone else in, when they text your ccchat number you'll get a notification in Signal with a ready-to-copy `/allow` command. Reply with it and they're approved.

---

## Useful Commands

Type any of these in the chat:

| Type this | What happens |
|-----------|--------------|
| `/help` | See all commands |
| `/status` | Check how much you've spent |
| `/reset` | Start a fresh conversation |
| `/remind 5m Check oven` | Get a reminder in 5 minutes |
| `/daily 09:00 Morning standup` | Get a message every day at 9am UTC |
| `/model sonnet` | Switch to a faster/cheaper AI model |

**Everything else** goes straight to Claude.

---

## Cost

ccchat is free. You pay for Claude AI usage via your Anthropic account — typically a few cents per conversation. Use `/status` to see your running total.

---

## Learn More

→ **[User Manual](USER_MANUAL.md)** — full setup guide, every command explained, troubleshooting

---

## License

MIT
