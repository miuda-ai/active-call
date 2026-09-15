# Telnyx Integration Guide for Active-Call

This guide explains how to configure active-call to work with Telnyx SIP trunking and AI services.

## Why Telnyx?

Telnyx provides a complete voice and AI platform that integrates seamlessly with active-call:

- **Global SIP Trunking**: Local presence in 140+ countries with carrier-grade reliability
- **Flexible Authentication**: IP-based authentication or SIP registration
- **Low-Latency Media**: Global edge network routes media optimally
- **TeXML Support**: TwiML-compatible programmable voice markup
- **53+ AI Models**: OpenAI-compatible API for LLM, ASR, and TTS
- **Unified Billing**: Single platform for voice and AI

## Quick Start

### 1. Create a Telnyx Account

Sign up at [telnyx.com](https://telnyx.com/sign-up) and verify your account.

### 2. Create a SIP Connection

1. Navigate to **Voice** > **SIP Connections** in the Portal
2. Click **Create SIP Connection**
3. Choose your authentication method:
   - **IP-based**: Add your server's public IP
   - **SIP Registration**: Note the username and password

### 3. Get Your API Key

1. Go to **API Keys** in the Portal
2. Create a new key with appropriate permissions
3. Save the key securely

### 4. Configure Active-Call

```bash
# Copy the example configuration
cp config/telnyx.example.toml config.toml

# Edit with your settings
nano config.toml

# Set your API key
export TELNYX_API_KEY="your-api-key-here"
```

### 5. Run Active-Call

```bash
# Start with Telnyx configuration
./active-call --conf config.toml
```

## SIP Configuration

### IP-Based Authentication

For IP-based authentication, Telnyx verifies connections by source IP:

```toml
# In config.toml
external_ip = "YOUR_SERVER_PUBLIC_IP"

# No SIP registration needed
# Just add your server IP to your Telnyx SIP Connection in the Portal
```

### SIP Registration

For SIP registration, configure your credentials:

```toml
[[register_users]]
server = "sip.telnyx.com:5060"
username = "YOUR_SIP_USERNAME"
disabled = false

[register_users.credential]
username = "YOUR_SIP_USERNAME"
password = "YOUR_SIP_PASSWORD"
```

### Telnyx SIP Domain

The primary SIP domain is `sip.telnyx.com`. For specific regions, Telnyx provides regional endpoints:

- **US East**: `sip.us-east.telnyx.com`
- **US West**: `sip.us-west.telnyx.com`
- **EU West**: `sip.eu-west.telnyx.com`
- **Asia Pacific**: `sip.ap-southeast.telnyx.com`

## AI Services Configuration

Active-call can use Telnyx AI APIs for ASR, LLM, and TTS:

### Playbook Configuration

```yaml
---
asr:
  provider: "openai"
  baseUrl: "https://api.telnyx.com/v2/ai"
  apiKey: "${TELNYX_API_KEY}"
  model: "whisper-1"
  language: "en"

tts:
  provider: "openai"
  baseUrl: "https://api.telnyx.com/v2/ai"
  apiKey: "${TELNYX_API_KEY}"
  model: "tts-1"
  voice: "alloy"

llm:
  provider: "openai"
  baseUrl: "https://api.telnyx.com/v2/ai"
  apiKey: "${TELNYX_API_KEY}"
  model: "gpt-4o-mini"
---
```

### Available AI Models

**Speech Recognition:**
- `whisper-1` (OpenAI Whisper)

**Text-to-Speech:**
- `tts-1` (optimized for speed)
- `tts-1-hd` (higher quality)
- Voices: `alloy`, `echo`, `fable`, `onyx`, `nova`, `shimmer`

**Language Models:**
- `gpt-4o`, `gpt-4o-mini`
- `claude-3-5-sonnet`
- `llama-3.1-70b-instruct`, `llama-3.1-8b-instruct`
- `mistral-large`, `mistral-small`
- 53+ models available

See the [Telnyx AI documentation](https://developers.telnyx.com/docs/ai/introduction) for the complete list.

## Making Outbound Calls

To make outbound calls through Telnyx:

```bash
# Using CLI
./active-call --call "sip:+18005551234@sip.telnyx.com" --handler greeting.md

# Using Playbook with refer
<refer to="sip:+18005551234@sip.telnyx.com" />
```

## WebRTC Integration

Telnyx provides a WebRTC SDK for browser-based voice:

1. Create a **Telnyx Call Control Application** in the Portal
2. Use the WebRTC SDK to connect browsers
3. Bridge WebRTC calls to active-call via SIP

See [Telnyx WebRTC documentation](https://developers.telnyx.com/docs/webrtc) for details.

## TeXML Support

Telnyx supports TeXML (TwiML-compatible markup) for programmable voice:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<Response>
  <Say>Hello from Telnyx!</Say>
  <Dial>sip:agent@your-server.com</Dial>
</Response>
```

Use TeXML for call flows that don't require real-time AI interaction.

## Troubleshooting

### SIP Registration Fails

1. Verify your SIP credentials in the Telnyx Portal
2. Check that `external_ip` is correctly configured
3. Ensure port 5060 (or your configured port) is accessible from the internet

### No Audio on Calls

1. Verify RTP ports are open: `rtp_start_port` to `rtp_end_port`
2. Check `external_ip` matches your public IP
3. Ensure your firewall allows UDP traffic for RTP

### AI API Errors

1. Verify `TELNYX_API_KEY` environment variable is set
2. Check API key permissions in the Portal
3. Confirm the model name is valid

### High Latency

1. Use regional SIP endpoints closer to your server
2. Check network path to Telnyx edge nodes
3. Consider using Telnyx AI APIs for lower-latency inference

## Additional Resources

- [Telnyx Voice Documentation](https://developers.telnyx.com/docs/voice)
- [Telnyx AI Documentation](https://developers.telnyx.com/docs/ai/introduction)
- [Telnyx API Reference](https://developers.telnyx.com/docs/api/v2/overview)
- [Telnyx Discord Community](https://discord.gg/telnyx)
- [Active-Call Documentation](./config_guide.en.md)

## License

This integration guide is provided under the same license as active-call.
