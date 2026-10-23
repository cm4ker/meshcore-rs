"""Drive the simulated companions with the official `meshcore` Python library (pip install meshcore).

Start `companion-sim` first (Sim0 on port 5000, Sim1 on 5001, repeater between them), then run
this script. Every step prints OK or FAIL; the exit code is the number of failures.
"""
import asyncio
import sys
import time

from meshcore import EventType, MeshCore

HOST = "127.0.0.1"
failures = 0


def check(name, cond, detail=""):
    global failures
    print(("OK   " if cond else "FAIL ") + name + (f"  ({detail})" if detail else ""), flush=True)
    if not cond:
        failures += 1


async def contact_named(mc, name):
    await mc.ensure_contacts(follow=True)
    for c in mc.contacts.values():
        if c["adv_name"] == name:
            return c
    return None


async def next_message(mc, timeout=5.0):
    """Wait for MESSAGES_WAITING (or poll), then fetch one message."""
    end = time.time() + timeout
    while time.time() < end:
        ev = await mc.commands.get_msg()
        if ev.type in (EventType.CONTACT_MSG_RECV, EventType.CHANNEL_MSG_RECV, EventType.CHANNEL_DATA_RECV):
            return ev
        await asyncio.sleep(0.2)
    return None


async def main():
    a = await MeshCore.create_tcp(HOST, 5000, only_error=True)
    b = await MeshCore.create_tcp(HOST, 5001, only_error=True)
    check("connect + app start (Sim0)", a is not None and a.self_info.get("name") == "Sim0", str(a and a.self_info))
    check("connect + app start (Sim1)", b is not None and b.self_info.get("name") == "Sim1")

    dev = await a.commands.send_device_query()
    check("device query", dev.type == EventType.DEVICE_INFO and dev.payload.get("fw ver") == 14, str(dev.payload))

    # adverts through the repeater
    for mc in (a, b):
        ev = await mc.commands.send_advert(flood=True)
        check("send flood advert", ev.type == EventType.OK)
    await asyncio.sleep(2)
    sim1 = await contact_named(a, "Sim1")
    sim0 = await contact_named(b, "Sim0")
    check("Sim0 learned Sim1 from its advert", sim1 is not None)
    check("Sim1 learned Sim0 from its advert", sim0 is not None)
    rep = await contact_named(a, "Sim Repeater")
    if rep is None:
        print("     (waiting for the repeater's boot advert)")
        await asyncio.sleep(17)
        rep = await contact_named(a, "Sim Repeater")
    check("repeater in Sim0's contacts", rep is not None and rep["type"] == 2)
    if not (sim1 and sim0 and rep):
        return

    # direct message with delivery confirmation
    sent = await a.commands.send_msg(sim1, "hello from the Python library")
    check("send_msg -> MSG_SENT (flood)", sent.type == EventType.MSG_SENT and sent.payload["type"] == 1, str(sent.payload))
    ack = await a.wait_for_event(EventType.ACK, attribute_filters={"code": sent.payload["expected_ack"].hex()}, timeout=5)
    check("ACK received for the message", ack is not None)
    msg = await next_message(b)
    check("Sim1 got the text", msg is not None and msg.payload.get("text") == "hello from the Python library",
          str(msg and msg.payload))
    check("one hop via the repeater", msg is not None and msg.payload.get("path_len") == 1)

    await asyncio.sleep(0.5)
    sim1 = await contact_named(a, "Sim1")
    check("route learned (out_path_len 1)", sim1["out_path_len"] == 1, str(sim1.get("out_path")))
    sent = await a.commands.send_msg(sim1, "second, direct")
    check("second message goes direct", sent.payload["type"] == 0)
    ack = await a.wait_for_event(EventType.ACK, attribute_filters={"code": sent.payload["expected_ack"].hex()}, timeout=5)
    check("ACK for the direct message", ack is not None)
    msg = await next_message(b)
    check("Sim1 got the direct text", msg is not None and msg.payload.get("text") == "second, direct")

    # public channel
    ev = await a.commands.send_chan_msg(0, "hi public")
    check("send_chan_msg", ev.type == EventType.OK)
    msg = await next_message(b)
    check("channel message arrives", msg is not None and msg.type == EventType.CHANNEL_MSG_RECV
          and msg.payload.get("text") == "Sim0: hi public", str(msg and msg.payload))

    # hashtag channel set by both
    for mc in (a, b):
        ev = await mc.commands.set_channel(2, "#rust")
        check("set_channel #rust", ev.type == EventType.OK)
    ch = await b.commands.get_channel(2)
    check("get_channel", ch.type == EventType.CHANNEL_INFO and ch.payload.get("channel_name") == "#rust", str(ch.payload))
    await a.commands.send_chan_msg(2, "on #rust")
    msg = await next_message(b)
    check("#rust message arrives on channel 2", msg is not None and msg.payload.get("channel_idx") == 2)

    # repeater: login, status, CLI
    login = await a.commands.send_login_sync(rep, "password")
    check("login to the repeater", login is not None and login.type == EventType.LOGIN_SUCCESS, str(login and login.payload))
    status = await a.commands.req_status_sync(rep)
    check("repeater status", status is not None and status.get("bat") == 4100, str(status))
    ev = await a.commands.send_cmd(rep, "get name")
    check("send_cmd", ev.type == EventType.MSG_SENT)
    msg = await next_message(a)
    check("CLI reply from the repeater", msg is not None and msg.payload.get("text") == "> Sim Repeater", str(msg and msg.payload))

    # telemetry and path discovery to Sim1
    ev = await b.commands.set_telemetry_mode_base(2)
    check("Sim1 allows telemetry", ev.type == EventType.OK)
    tel = await a.commands.req_telemetry_sync(sim1)
    check("telemetry from Sim1", tel is not None and any(t.get("type") == "voltage" for t in tel), str(tel))
    path = await a.commands.send_path_discovery_sync(sim1)
    check("path discovery", path is not None and path.type == EventType.PATH_RESPONSE, str(path and path.payload))

    # trace through the repeater
    ev = await a.commands.send_trace(path=rep["public_key"][:2])
    check("send_trace", ev.type == EventType.MSG_SENT)
    tr = await a.wait_for_event(EventType.TRACE_DATA, timeout=5)
    check("trace data", tr is not None and tr.payload.get("path_len") == 1, str(tr and tr.payload))

    # misc device commands
    ev = await a.commands.run_cli_command("get radio")
    check("run_cli_command", ev.type == EventType.CLI_REPLY, str(ev.payload))
    for getter in ("get_bat", "get_time", "get_stats_core", "get_stats_radio", "get_stats_packets", "get_tuning",
                   "get_autoadd_config", "get_allowed_repeat_freq", "get_custom_vars"):
        ev = await getattr(a.commands, getter)()
        check(getter, ev.type != EventType.ERROR, str(ev.payload)[:80])
    card = await a.commands.export_contact()
    check("export own contact card", card.type == EventType.CONTACT_URI, str(card.payload)[:60])
    sig = await a.commands.sign(b"sign me")
    check("sign", sig.type == EventType.SIGNATURE)
    ev = await a.commands.set_name("Sim0-renamed")
    check("set_name", ev.type == EventType.OK)
    info = await a.commands.send_appstart()
    check("name changed", info.payload.get("name") == "Sim0-renamed")

    await a.disconnect()
    await b.disconnect()


asyncio.run(main())
print(f"\n{failures} failure(s)")
sys.exit(failures)
