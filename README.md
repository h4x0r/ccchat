# ccchat - Claude Code Chat

Chat with Claude AI directly from your favourite messenger. Send a message, get an intelligent response — no browser or app switching required.

## What is ccchat?

ccchat brings Claude AI into your messaging apps. Instead of opening a separate tool, just text your questions and Claude replies in the same chat.

- Ask questions, get answers — right where you message
- Each person gets their own private conversation that remembers context
- Choose between different Claude models (Opus, Sonnet, Haiku)
- Built-in cost tracking so you stay within budget

## Supported Messengers

| Messenger | Status |
|-----------|--------|
| Signal | Supported |

More messengers coming soon.

## How it Works

```
You (messenger) → ccchat → Claude AI → ccchat → You (messenger)
```

You send a message. ccchat picks it up, asks Claude, and sends the answer back. That's it.

## Quick Start (Signal)

### What You Need

1. **A Signal account** linked to [signal-cli](https://github.com/AsamK/signal-cli)
2. **[signal-cli-api](https://github.com/h4x0r/signal-cli-api)** running on your machine
3. **[Claude Code](https://docs.anthropic.com/en/docs/claude-code)** installed and logged in

### Install

```bash
cargo install ccchat
```

### Run

```bash
# Replace with your actual number
ccchat --account +447700000000
```

- `--account` is the number ccchat listens on
- By default, only messages from your own number are accepted (notes to self)
- Use `--allowed` to add other numbers: `--allowed +447700000001,+447700000002`

You can also use environment variables instead of flags:

```bash
export CCCHAT_ACCOUNT=+447700000000
export CCCHAT_ALLOWED=+447700000001
ccchat
```

Copy `.env.example` to `.env` for a template.

## Using ccchat

Once running, just send a message to your ccchat number from your phone. Claude will respond in the same chat.

### Commands

Type these in your chat to control ccchat:

| Command | What it Does |
|---------|-------------|
| `/reset` | Start a fresh conversation (clears memory) |
| `/status` | Show how long ccchat has been running, message count, and total cost |
| `/model sonnet` | Switch to a different Claude model (opus, sonnet, haiku) |

Everything else you type gets sent to Claude.

## Options

| Setting | Default | What it Does |
|---------|---------|-------------|
| `--account` | (required) | Your account identifier |
| `--allowed` | your own number | Who's allowed to chat |
| `--model` | opus | Which Claude model to use |
| `--max-budget` | $5.00 | Maximum spend per message |
| `--api-url` | localhost:8080 | Messenger API endpoint |

## How Much Does it Cost?

ccchat itself is free. You pay for Claude API usage through your Anthropic subscription. Use `/status` to check your running total, and `--max-budget` to set a per-message spending cap.

## Troubleshooting

**ccchat starts but I don't get replies**
- Make sure your messenger API is running
- Check that `claude` works on its own (`claude -p "hello"`)
- Verify your account number is correct

**"Ignoring message from non-allowed sender"**
- The sender isn't in your `--allowed` list. Add their number with `--allowed`.

**Messages are cut off**
- Long responses are automatically split into multiple messages. They should arrive in order.

## License

MIT
