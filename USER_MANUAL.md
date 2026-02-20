# ccchat User Manual

Complete reference for setting up and using ccchat — Claude AI in your Signal messenger.

---

## Table of Contents

1. [How ccchat Works](#how-ccchat-works)
2. [Setup Guide](#setup-guide)
3. [Chatting with Claude](#chatting-with-claude)
4. [Commands Reference](#commands-reference)
   - [General](#general-commands)
   - [Reminders](#reminders)
   - [Recurring Jobs](#recurring-jobs)
   - [Conversation Pins](#conversation-pins)
   - [Admin](#admin-commands)
5. [Managing Who Can Chat](#managing-who-can-chat)
6. [AI Models](#ai-models)
7. [Configuration Options](#configuration-options)
8. [Monitoring & Stats](#monitoring--stats)
9. [Troubleshooting](#troubleshooting)
10. [Cost & Billing](#cost--billing)

---

## How ccchat Works

```
Your phone  ──Signal──►  ccchat  ──►  Claude AI  ──►  ccchat  ──Signal──►  Your phone
```

ccchat runs on a computer (or server) connected to Signal via [signal-cli](https://github.com/AsamK/signal-cli). It receives your messages, sends them to Claude AI, and returns Claude's response — all via Signal.

Each approved sender has their own private conversation with their own memory. Claude remembers context within a session and summarises important points for future sessions.

---

## Setup Guide

### Step 1: Install Prerequisites

**On macOS:**
```bash
# Install Homebrew if you don't have it
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"

# Install Rust (needed for ccchat)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install Java (needed for signal-cli)
brew install openjdk
```

**On Linux (Ubuntu/Debian):**
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
sudo apt install default-jre
```

### Step 2: Register a Signal Number with signal-cli

You need a dedicated phone number for ccchat. This can be:
- A spare SIM card
- A VoIP number (e.g. Google Voice, Twilio)
- A second device

```bash
# Download signal-cli (check for latest version at https://github.com/AsamK/signal-cli/releases)
wget https://github.com/AsamK/signal-cli/releases/latest/download/signal-cli-*.tar.gz
tar -xf signal-cli-*.tar.gz
sudo mv signal-cli-* /opt/signal-cli
sudo ln -s /opt/signal-cli/bin/signal-cli /usr/local/bin/signal-cli

# Register your number (replace with your number)
signal-cli -a +447700000000 register

# Verify with the SMS code you receive
signal-cli -a +447700000000 verify 123456
```

### Step 3: Install Claude Code

```bash
npm install -g @anthropic-ai/claude-code
claude login
```

Follow the prompts to log in with your Anthropic account.

### Step 4: Install ccchat

```bash
cargo install ccchat
```

### Step 5: Start ccchat

```bash
ccchat --account +447700000000
```

ccchat will automatically set up the Signal API bridge and start listening for messages.

### Step 6: Test It

From your personal Signal number, send a message to your ccchat number. You should get a reply from Claude within a few seconds.

### Using a .env File

Instead of flags, you can use a `.env` file:

```bash
cp .env.example .env
# Edit .env with your settings
```

```env
CCCHAT_ACCOUNT=+447700000000
CCCHAT_MODEL=opus
CCCHAT_MAX_BUDGET=5.00
```

Then just run `ccchat` with no flags.

---

## Chatting with Claude

Just type normally — anything that isn't a command (starting with `/`) goes straight to Claude.

**Examples of what you can ask:**

- "Summarise this article for me: [paste text]"
- "Write a professional reply to this email: [paste email]"
- "What's the weather like in Tokyo in March?"
- "Help me debug this code: [paste code]"
- "Give me 5 ideas for a birthday gift for my mum"
- "Translate this to Spanish: [text]"

Claude remembers the context of your conversation, so you can have back-and-forth exchanges:

> You: What's photosynthesis?
> Claude: Photosynthesis is...
> You: Can you explain it more simply for a 10-year-old?
> Claude: Sure! Think of it like this...

### Long Responses

If Claude's response is very long, it will be split into multiple messages. If a response is cut short, type `/more` to get the continuation.

---

## Commands Reference

Type commands directly in your Signal chat. All commands start with `/`.

### General Commands

| Command | Description |
|---------|-------------|
| `/help` | Show a summary of all available commands |
| `/status` | Show uptime, total messages, total cost, and average response time |
| `/usage` | Show your personal usage stats (messages sent, cost) |
| `/reset` | End the current conversation session. Claude saves a summary of what you discussed, then starts fresh |
| `/more` | Continue a response that was cut short |
| `/model <name>` | Switch the AI model for your conversation (see [AI Models](#ai-models)) |
| `/memory` | Show the conversation summaries Claude has stored about your past sessions |
| `/forget` | Delete all stored memory for your account |
| `/search <query>` | Search your conversation history for a keyword or phrase |
| `/export` | Export your full conversation history as a text file |

### Reminders

Set one-time reminders and ccchat will message you when the time comes.

| Command | Description | Example |
|---------|-------------|---------|
| `/remind <time> <message>` | Set a reminder | `/remind 30m Take pasta off the stove` |
| `/reminders` | List all your pending reminders with their IDs | |
| `/cancel <id>` | Cancel a reminder by its ID | `/cancel 3` |

**Time formats:**
- `5m` — 5 minutes from now
- `2h` — 2 hours from now
- `1d` — 1 day from now
- `90s` — 90 seconds from now

All times are relative to when you send the command.

**Examples:**
```
/remind 1h Take the chicken out of the oven
/remind 2d Submit quarterly report
/remind 15m Stand up from your desk and stretch
```

### Recurring Jobs

Set up messages that repeat on a schedule. All times are **UTC**.

#### Simple Intervals — `/every`

Repeat a message every N time units.

```
/every <interval> <message>
```

| Example | What it does |
|---------|--------------|
| `/every 1h Check on the downloads` | Message every hour |
| `/every 30m Drink some water` | Message every 30 minutes |
| `/every 1d Daily reminder to log your hours` | Message every 24 hours |

#### Daily at a Fixed Time — `/daily`

Send a message every day at a specific UTC time.

```
/daily <HH:MM> <message>
```

| Example | What it does |
|---------|--------------|
| `/daily 09:00 Morning standup in 15 minutes` | Every day at 9:00 AM UTC |
| `/daily 17:00 End of day — update your task list` | Every day at 5:00 PM UTC |

> **Note:** Times are UTC. If you're in London (UTC+0 in winter, UTC+1 in summer), `/daily 09:00` fires at 9am in winter and 10am British Summer Time.

#### Custom Cron Schedule — `/cron`

For advanced scheduling using standard cron expressions.

```
/cron "<cron pattern>" <message>
```

A cron pattern has 5 fields: `minute hour day-of-month month day-of-week`

| Example | What it does |
|---------|--------------|
| `/cron "0 9 * * MON" Weekly team standup` | Every Monday at 9:00 AM UTC |
| `/cron "0 8 1 * *" First of the month — pay rent` | 1st of every month at 8:00 AM |
| `/cron "30 12 * * FRI" Friday lunchtime reminder` | Every Friday at 12:30 PM UTC |
| `/cron "0 */6 * * *" Server health check` | Every 6 hours |

**Cron quick reference:**

| Field | Values |
|-------|--------|
| Minute | 0–59 |
| Hour | 0–23 |
| Day of month | 1–31 |
| Month | 1–12 |
| Day of week | MON, TUE, WED, THU, FRI, SAT, SUN (or 0–6) |
| `*` | Every value |
| `*/N` | Every N values |

#### Managing Recurring Jobs

| Command | Description | Example |
|---------|-------------|---------|
| `/crons` | List all your active recurring jobs with IDs and schedules | |
| `/cron-cancel <id>` | Permanently delete a job | `/cron-cancel 2` |
| `/cron-pause <id>` | Pause a job (keeps it but stops firing) | `/cron-pause 2` |
| `/cron-resume <id>` | Resume a paused job | `/cron-resume 2` |

### Conversation Pins

Save snippets of your conversation to recall later — useful for referencing important decisions, research, or context.

| Command | Description | Example |
|---------|-------------|---------|
| `/pin <label>` | Pin the last few messages with a label | `/pin project-plan` |
| `/pins` | List all your saved pins | |
| `/recall <label>` | Bring a pinned conversation back into context | `/recall project-plan` |

**Workflow example:**
1. Have a conversation about a project plan
2. Type `/pin project-plan` to save it
3. Later, start a new conversation and type `/recall project-plan` to give Claude the context from that earlier discussion

### Admin Commands

These commands control who can use ccchat and monitor activity. Run them from your own Signal number (the account owner).

| Command | Description |
|---------|-------------|
| `/allow <id>` | Approve a sender so they can chat with Claude |
| `/revoke <id>` | Remove a sender's access |
| `/pending` | Show people who have messaged but haven't been approved yet |
| `/audit` | View a log of recent admin actions (approvals, revocations) |
| `/export-config` | Export the allowed senders list as JSON (for backup or migration) |

**Approving a new sender:**

When someone messages your ccchat number for the first time, you'll receive a notification in your Note to Self chat:

```
New message from Alice (+447711111111)
To approve: /allow +447711111111
```

Reply with the `/allow` command to grant them access. They'll never know they were waiting — their original message will be processed immediately.

---

## Managing Who Can Chat

By default, only your own number can use ccchat. This keeps it private.

### Approving People

Two ways to approve someone:
1. **Wait for a notification** — send them your ccchat number, and when they message you, approve via the notification
2. **Pre-approve** — type `/allow +447711111111` before they message

### Revoking Access

```
/revoke +447711111111
```

Their future messages will be blocked. They won't be notified.

### Viewing Pending Requests

```
/pending
```

Lists everyone who has messaged but not been approved yet.

### Persistent Storage

Approved senders are saved to `~/.config/ccchat/allowed.json` and survive restarts. You can also edit this file directly or import it on a new machine.

---

## AI Models

ccchat supports three Claude models. Switch any time with `/model <name>`.

| Model | Speed | Cost | Best For |
|-------|-------|------|----------|
| `opus` | Slower | Higher | Complex reasoning, long documents, creative tasks |
| `sonnet` | Fast | Medium | Most everyday tasks — good balance |
| `haiku` | Fastest | Lowest | Quick questions, simple tasks |

**Default:** `opus`

**Switching model:**
```
/model sonnet
/model haiku
/model opus
```

Your model preference is saved per-account and persists across sessions.

---

## Configuration Options

Pass these when starting ccchat, or set them via environment variables.

| Flag | Env Variable | Default | Description |
|------|-------------|---------|-------------|
| `--account` | `CCCHAT_ACCOUNT` | *(required)* | Your Signal account number (e.g. `+447700000000`) |
| `--model` | `CCCHAT_MODEL` | `opus` | Default Claude model |
| `--max-budget` | `CCCHAT_MAX_BUDGET` | `5.00` | Max USD to spend per message |
| `--port` | `CCCHAT_PORT` | `8080` | Port for the internal Signal API bridge |
| `--api-url` | `CCCHAT_API_URL` | *(auto)* | Use an external signal-cli-rest-api instead of the built-in one |

**Example `.env` file:**

```env
CCCHAT_ACCOUNT=+447700000000
CCCHAT_MODEL=sonnet
CCCHAT_MAX_BUDGET=2.00
```

---

## Monitoring & Stats

### Chat Commands

- `/status` — uptime, message count, total cost, average response time
- `/usage` — your personal stats

### HTTP Endpoints

ccchat exposes a small HTTP server (on the stats port, default `8081`) for monitoring:

| Endpoint | Format | Description |
|----------|--------|-------------|
| `/` | JSON | Full stats (same as `/status`) |
| `/healthz` | JSON | Health check — returns `{"status":"ok"}` |
| `/metrics` | Prometheus | Metrics in Prometheus text format |

These are useful if you run ccchat on a server and want to hook it into uptime monitoring or dashboards.

---

## Troubleshooting

### ccchat starts but I don't receive any replies

1. Check Claude Code is working independently:
   ```bash
   claude -p "say hello"
   ```
   If this fails, re-run `claude login`.

2. Check your Signal account number is correct (include the `+` and country code).

3. Check ccchat's logs for errors — they print to the terminal where ccchat is running.

### I sent a message but nothing happened

- Your number may not be approved yet. Check Note to Self for a pending approval notification, or run `/pending` from your own number.
- If you *are* approved, check the terminal for error messages.

### Messages are cut off or truncated

- Long responses are automatically split across multiple Signal messages. They arrive in order.
- If a response was cut mid-sentence, type `/more` to get the rest.

### ccchat won't start — "port already in use"

ccchat auto-selects a port if 8080 is taken. If you want a specific port:
```bash
ccchat --account +447700000000 --port 8090
```

### signal-cli errors on startup

Make sure you've completed signal-cli registration:
```bash
signal-cli -a +447700000000 receive
```
If it errors, your registration may have expired. Re-register with the same number.

### Reminders or cron jobs aren't firing

- Confirm the time has passed (all times are UTC)
- Run `/reminders` or `/crons` to verify the job exists and its status is active
- Check ccchat is still running (it needs to be up to deliver scheduled messages)

---

## Cost & Billing

ccchat itself is **free and open source**.

You pay Anthropic for Claude API usage via your Anthropic account. Costs are per-message based on the length of input + output.

**Rough estimates:**
- Short question + short answer: < $0.01
- Summarising a long document: $0.01–$0.05
- Complex multi-turn conversation: $0.05–$0.25

**Tools to manage cost:**
- `/status` — see your total spend since ccchat started
- `/usage` — your personal spend
- `--max-budget` — cap the maximum spend per single message (default $5.00)
- `/model haiku` — switch to the cheapest model for simple tasks

---

## Data & Privacy

- Conversation history is stored locally on the machine running ccchat, in `~/.config/ccchat/`
- Nothing is sent anywhere except to Anthropic's API (for Claude) and Signal's servers (for messaging)
- Each sender's memory is stored in a separate database, identified by a hash of their phone number
- You can delete all stored memory with `/forget`

---

## License

MIT — free to use, modify, and distribute.
