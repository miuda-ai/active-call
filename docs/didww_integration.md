# DIDWW Integration Guide

This guide explains how to connect `active-call` to [DIDWW](https://www.didww.com/) SIP trunking for inbound and outbound calls. DIDWW uses standard SIP, so no SDK is required.

## How It Works

```
   Phone network (PSTN)
          │
        DIDWW
          │
    SIP/UDP 5060 + RTP
          │
   active-call (Rust)
          │
     AI Pipeline (ASR → LLM → TTS)
```

- **Inbound**: DIDWW sends calls to your DID straight to the public IP of `active-call`. No SIP registration is needed.
- **Outbound**: `active-call` sends calls to the DIDWW Voice OUT trunk and authenticates with a username and password. Your DID is used as the caller ID.

The examples use offline ASR (SenseVoice) and TTS (Supertonic) with an OpenAI LLM.

---

## Prerequisites

- A DIDWW account with a DID number
- A server with a public IP address; UDP 5060 and the RTP port range open. Keep TCP 8080 closed to the internet: the HTTP API has no authentication.
- Docker
- An OpenAI API key
- `active-call` v0.3.117 or later (the `ghcr.io/miuda-ai/active-call:latest` image)
- A clone of this repository; run the commands from its root

---

## Step 1: Configure DIDWW

### 1.1 Inbound SIP Trunk

[Create a SIP trunk](https://doc.didww.com/voice/inbound-trunks/sip-trunks/how-to-guides/create-sip-trunk.html) with these settings:

- **Type**: Static Endpoint
- **Host**: `YOUR_PUBLIC_IP`
- **Port**: `5060`, **Transport**: UDP

### 1.2 Outbound Trunk

Outbound trunks require [access approval](https://doc.didww.com/voice/outbound-trunks/get-access.html). Then [create an outbound trunk](https://doc.didww.com/voice/outbound-trunks/how-to-guides/create-outbound-trunk.html) with these settings:

- **Authentication method**: Credentials & IP-Based
- **Allowed SIP IP addresses**: `YOUR_PUBLIC_IP/32`
- **CLI settings**: allow your DID as caller ID

After the trunk is created, copy its [username and password](https://doc.didww.com/voice/outbound-trunks/how-to-guides/view-outbound-trunk-credentials.html). Choose the nearest endpoint from [Outbound SIP information](https://doc.didww.com/voice/outbound-trunks/outbound-sip-information.html). The examples use `fra.eu.out.didww.com`.

### 1.3 Assign the DID

[Assign your DID](https://doc.didww.com/phone-numbers/my-numbers/how-to-guides/assign-voice-trunk.html) to the inbound trunk. The DID needs channel capacity to carry calls.

---

## Step 2: Configure active-call

Copy the example configuration and fill in your values:

```bash
cp config/didww.example.toml config.toml
```

```toml
# DIDWW SIP trunk configuration
# Guide: docs/didww_integration.md

addr = "0.0.0.0"
http_addr = "0.0.0.0:8080"
udp_port = 5060

# Your server's public IP
external_ip = "YOUR_PUBLIC_IP"

# RTP ports; open this UDP range in your firewall
rtp_start_port = 35000
rtp_end_port = 35100

# Playbook for inbound calls
[handler]
type = "playbook"
default = "didww_inbound.md"

# Outbound trunk credentials. active-call uses them for calls to this host.
# DIDWW does not require registration for outbound calls.
[[register_users]]
server = "fra.eu.out.didww.com"
username = "YOUR_DIDWW_USERNAME"
disabled = true

[register_users.credential]
username = "YOUR_DIDWW_USERNAME"
password = "YOUR_DIDWW_PASSWORD"

# DIDWW requires your DID as the caller ID; the host replaces the default 127.0.0.1
[[trunk_rules]]
rule.match.to.host = "^fra\\.eu\\.out\\.didww\\.com$"
rule.rewrite.from.user = "YOUR_DID"
rule.rewrite.from.host = "fra.eu.out.didww.com"
```

- **`external_ip`**: your server's public IP, used in SIP and SDP.
- **`[[register_users]]`**: the outbound trunk credentials. `active-call` uses them for calls to `server`. `disabled = true` turns off SIP registration, which DIDWW does not require for outbound calls. Versions before v0.3.118 register anyway; this does no harm.
- **`[[trunk_rules]]`**: sets your DID as the caller ID on outbound calls, in E.164 format without `+` (for example `442039968519`). Without it, DIDWW rejects the call with `404 No network found for src number`.
- If you use another DIDWW endpoint, change it in both `server` and `rule.match.to.host`.

---

## Step 3: Playbooks

Playbooks are in `config/playbook/`. The model and API settings are in the front matter, and the prompt follows it.

### Inbound: `didww_inbound.md`

```markdown
---
asr:
  provider: "sensevoice"
  language: "en"
llm:
  provider: "openai"
  model: "gpt-4o-mini"
  language: "en"
tts:
  provider: "supertonic"
  speaker: "F1"
  language: "en"
vad:
  silencePadding: 700
  speechPadding: 300
denoise: true
---
You are a friendly phone assistant. Greet the caller once and ask how you can help.

- Keep every answer short. Speak English only.
- When the caller says goodbye, say goodbye and output <hangup/> to end the call.
```

The playbook has no `greeting`: the LLM greets the caller.

### Outbound: `didww_outbound.md`

```markdown
---
asr:
  provider: "sensevoice"
  language: "en"
llm:
  provider: "openai"
  model: "gpt-4o-mini"
  language: "en"
tts:
  provider: "supertonic"
  speaker: "F1"
  language: "en"
vad:
  silencePadding: 700
  speechPadding: 300
denoise: true
greeting: "Hello, this is Anna from Bright Smile Dental Clinic. I am calling to remind you about your dental check-up tomorrow at 10 AM. Can you confirm you will come?"
interruption:
  strategy: "none"
---
You are Anna, an assistant from Bright Smile Dental Clinic. You placed this outbound call to remind the person about their dental check-up tomorrow at 10 AM and to confirm they will come.

- The call starts with a fixed greeting that already introduced you and the reason for the call. Do not introduce yourself again; if the person interrupts the greeting, just ask whether they can confirm the appointment. Never ask how you can help: you called them.
- Ask them to confirm the appointment. If they cannot come, offer tomorrow at 3 PM or Friday at 11 AM instead.
- Speech recognition may give you fragments or noise. If a message is a single word or makes no sense, briefly repeat your question instead of saying you did not understand.
- Keep every answer short. Speak English only.
- When the appointment is confirmed or rescheduled, thank them, say goodbye and output <hangup/> to end the call.
```

- **`greeting`**: plays after the callee answers. Without it, the LLM starts speaking before the answer and the callee misses the beginning.
- **`interruption.strategy: "none"`**: with other strategies, if the callee speaks during the greeting, the greeting keeps playing over the reply.

### Settings in both playbooks

- **`llm.language: "en"`**: the built-in tool instructions, including `<hangup/>`, default to Chinese. With English instructions the agent ends the call when asked.
- **`vad`**: `silencePadding` longer than the default 100 ms keeps phrases whole, so ASR gets complete sentences instead of fragments.

---

## Step 4: Run active-call

Download the offline ASR and TTS models (about 480 MB, once):

```bash
docker run --rm -v $(pwd)/data/models:/models \
  ghcr.io/miuda-ai/active-call:latest \
  --download-models all --models-dir /models --exit-after-download
```

Start `active-call`:

```bash
docker run -d --name active-call --net host \
  -e OPENAI_API_KEY="your-openai-key" \
  -v $(pwd)/config.toml:/app/config.toml:ro \
  -v $(pwd)/config/playbook:/app/config/playbook \
  -v $(pwd)/data/models:/app/models \
  ghcr.io/miuda-ai/active-call:latest
```

To test inbound calls, call your DID from any phone. The agent answers and greets you.

---

## Step 5: Make Outbound Calls

Stop the running container and start one that places a call. Use the number in E.164 format without `+`:

```bash
docker stop active-call
docker run --rm -it --net host \
  -e OPENAI_API_KEY="your-openai-key" \
  -v $(pwd)/config.toml:/app/config.toml:ro \
  -v $(pwd)/config/playbook:/app/config/playbook \
  -v $(pwd)/data/models:/app/models \
  ghcr.io/miuda-ai/active-call:latest \
  --conf /app/config.toml \
  --call "sip:PHONE_NUMBER@fra.eu.out.didww.com" \
  --handler didww_outbound.md
```

`--handler` also sets the playbook for inbound calls, so this container answers inbound calls with `didww_outbound.md`. After the call, stop it with Ctrl+C and run `docker start active-call`.

To place calls from your application, use the [WebSocket API](api.md).

---

## Troubleshooting

### Inbound calls do not reach active-call

- Check that the inbound trunk **Host** is your public IP and UDP port 5060 is open.
- Check that the DID is assigned to the inbound trunk.

### Audio is one-way or silent

- Check that `external_ip` is your public IP.
- Open the RTP port range (`rtp_start_port`–`rtp_end_port`) for UDP in your firewall.

### Outbound calls fail with 401 Unauthorized

- The first `401` is the normal digest challenge; `active-call` retries with credentials.
- If the retry also fails, check the username and password in `[register_users.credential]`.
- `server` must match the host in the `--call` URI, or the credentials are not used.

### Outbound calls fail with 404 No network found for src number

- The caller ID is not your DID. Check `rule.rewrite.from.user` in `[[trunk_rules]]` and that `rule.match.to.host` matches the endpoint.

### Outbound calls to your own DID fail with 403

- Calling your own DID through the outbound trunk returns `403 Destination number blacklisted by routing plan`. Test with another number.

### The greeting and the reply play at the same time

- Set `interruption.strategy: "none"` in the outbound playbook.

### The agent does not end the call

- Set `llm.language: "en"` and ask for `<hangup/>` in the prompt.

---
