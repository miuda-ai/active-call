# DIDWW SIP trunking

Connect Active Call to DIDWW using its existing SIP transport and call-control
API. Inbound delivery and outbound authentication are configured separately;
no DIDWW SDK or changes to Active Call are required for these examples.

## Prerequisites

- A DIDWW account with an active inbound DID and, for outgoing calls, enabled
  Outbound Trunks service, an authorized Caller ID and sufficient call allowance.
- A stable public SIP/RTP address reachable by DIDWW. When using NAT, forward the
  configured SIP and RTP ports and advertise that public address in `external_ip`.
- A running Active Call controller or configured playbook. SIP connectivity does
  not configure ASR, TTS or LLM services: supply their models, credentials and
  media assets separately.

The examples use UDP SIP and plain RTP with G.711 PCMU/PCMA. Match the DIDWW trunk
settings to this profile. For TLS/SRTP, configure certificates and the supported
encryption settings separately and verify them before use.

## Inbound: DIDWW to Active Call

1. Copy `config/didww-inbound.example.toml` to a local configuration file. Replace
   `YOUR_PUBLIC_IP`, adjust the SIP/RTP ports and set `[handler].url` to your
   existing controller's invite webhook.
2. In the DIDWW User Panel, configure an inbound SIP trunk with **Static Endpoint**
   routing. Set its host to your public SIP address, port to `udp_port`, transport
   to UDP and Request-URI user part to `{DID}`. Assign the inbound DID to this trunk.
3. Leave optional digest authentication from DIDWW to your endpoint disabled for
   this profile. Restrict incoming SIP and RTP to DIDWW's documented networks in
   your firewall. Active Call's `options_response.allow` controls OPTIONS replies;
   it is not an access-control list for incoming INVITEs.
4. Start Active Call with `./active-call --conf <local-config.toml>`.

No `register_users` entry is needed for Static Endpoint delivery. The controller
receives an `invite` webhook containing `dialogId`, then connects to
`ws://127.0.0.1:8080/call/sip?id=<dialogId>&dump_events=false` and sends:

```json
{
  "command": "accept",
  "option": {}
}
```

Keep that WebSocket open and use the existing [call/media commands](./api.md)
to handle audio. An empty accept option establishes the SIP call; it does not
generate a greeting or an AI conversation. The webhook must respond successfully
within `accept_timeout`.

Alternatively, replace the webhook handler with your existing playbook handler.
The shipped `hello.md` needs its local speech models, LLM configuration and media
assets. Do not assume it works without those dependencies.

## Outbound: Active Call to DIDWW

1. Copy `config/didww-outbound.example.toml` to a local configuration file and
   replace `YOUR_PUBLIC_IP`. Start Active Call with that file. The inbound example
   can also be used when the same instance handles both directions.
2. Configure a DIDWW outbound trunk using **Credentials & IP-Based** authentication.
   Allow the application's public signaling and media addresses, enable the
   required destinations and allow the Caller ID you will present.
3. Obtain the trunk's SIP username/password and select an outbound gateway from
   DIDWW's current SIP information. Keep credentials in your controller's local
   secret store; never commit them to TOML or send them to a browser client.
4. Open `ws://127.0.0.1:8080/call/sip?dump_events=false` from your trusted server-side
   controller, then send the following command after substituting placeholders:

```json
{
  "command": "invite",
  "option": {
    "caller": "sip:<AUTHORIZED_E164_CLI>@<CALLER_DOMAIN>",
    "callee": "sip:<DESTINATION_E164>@<DIDWW_OUTBOUND_GATEWAY>",
    "sip": {
      "username": "<DIDWW_OUTBOUND_USERNAME>",
      "password": "<DIDWW_OUTBOUND_PASSWORD>",
      "realm": "out.didww.com"
    }
  }
}
```

The digest username and the caller number serve different purposes. Set `caller`
explicitly to a DIDWW-authorized E.164 CLI; it must not default to the digest
username. Use the fixed digest realm `out.didww.com`, even with a regional gateway.
Keep the WebSocket open and supply your normal audio/AI handling after answer.

Outbound INVITEs do not require SIP registration. Do not add a disabled
`register_users` entry to hold these credentials: the startup registration path
does not filter that flag at the documented source baseline. Per-call `sip`
options avoid unnecessary REGISTER traffic and let inbound/outbound credentials
remain separate.

Keep the HTTP/WebSocket control endpoint private. The examples bind it to
loopback; if a controller runs in another container, use a private network and
appropriate access controls rather than publishing the endpoint directly.

## Verify each direction

First check the substituted configuration in an isolated SIP environment. For
each direction, establish and answer one call, check SDP addresses and negotiated
codec, exchange a known audio signal both ways, and verify clean hangup. Check
outbound digest authentication as well. RTP packet counters help diagnose media;
they do not establish that the expected audio was decoded or heard.

Then make a separately approved DIDWW call in each direction to a controlled
destination. Confirm the presented Caller ID, audible application audio and
audio returning to the application. Use bounded call duration and spending
limits. If verification uses a SIP relay, record which hop handles digest and
media; a relay-backed result does not establish direct carrier interoperability.

For silent or one-way calls, compare the SDP/public address, NAT mapping, allowed
media networks and RTP port range. For outbound 401 failures, check credentials
and realm. For 403 failures, also check allowed source IPs, CLI, destination and
trunk limits. Do not log actual credentials or full call-control payloads.

## References

- [DIDWW inbound SIP routing](https://doc.didww.com/voice/inbound-trunks/sip-trunks/sip-routing-methods.html)
- [DIDWW outbound authentication](https://doc.didww.com/voice/outbound-trunks/authentication-security.html)
- [DIDWW outbound SIP information](https://doc.didww.com/voice/outbound-trunks/outbound-sip-information.html)
- [DIDWW outbound dialing](https://doc.didww.com/voice/outbound-trunks/routing-dialing/outbound-dialing.html)
- [Active Call API](./api.md)
