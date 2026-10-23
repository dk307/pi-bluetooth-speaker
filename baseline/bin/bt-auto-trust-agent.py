#!/usr/bin/env python3
import sys
import signal
import dbus
import dbus.service
import dbus.mainloop.glib
from gi.repository import GLib

AGENT_PATH = "/test/agent"

class AutoTrustAgent(dbus.service.Object):
    def __init__(self, bus, path):
        super().__init__(bus, path)
        self.bus = bus

    def _set_trusted(self, path):
        try:
            dev = self.bus.get_object("org.bluez", path)
            props = dbus.Interface(dev, "org.freedesktop.DBus.Properties")
            props.Set("org.bluez.Device1", "Trusted", dbus.Boolean(True))
            print(f"[Agent] Automatically set {path} to Trusted: yes", flush=True)
        except Exception as e:
            print(f"[Agent] Failed to trust {path}: {e}", flush=True)

    @dbus.service.method("org.bluez.Agent1", in_signature="", out_signature="")
    def Release(self):
        print("[Agent] Release requested by BlueZ", flush=True)

    @dbus.service.method("org.bluez.Agent1", in_signature="os", out_signature="")
    def AuthorizeService(self, device, uuid):
        self._set_trusted(device)
        return

    @dbus.service.method("org.bluez.Agent1", in_signature="o", out_signature="s")
    def RequestPinCode(self, device):
        self._set_trusted(device)
        return "0000"

    @dbus.service.method("org.bluez.Agent1", in_signature="o", out_signature="u")
    def RequestPasskey(self, device):
        self._set_trusted(device)
        return dbus.UInt32(0)

    @dbus.service.method("org.bluez.Agent1", in_signature="ou", out_signature="")
    def RequestConfirmation(self, device, passkey):
        self._set_trusted(device)
        return

    @dbus.service.method("org.bluez.Agent1", in_signature="o", out_signature="")
    def RequestAuthorization(self, device):
        self._set_trusted(device)
        return

def cleanup(signum, frame):
    print(f"\n[Agent] Caught signal {signum}, unregistering agent...", flush=True)
    try:
        manager.UnregisterAgent(AGENT_PATH)
        print("[Agent] Successfully unregistered agent from BlueZ", flush=True)
    except Exception as e:
        print(f"[Agent] Cleanup exception: {e}", flush=True)
    finally:
        if loop and loop.is_running():
            loop.quit()
    sys.exit(0)

if __name__ == "__main__":
    dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
    bus = dbus.SystemBus()
    agent = AutoTrustAgent(bus, AGENT_PATH)

    obj = bus.get_object("org.bluez", "/org/bluez")
    manager = dbus.Interface(obj, "org.bluez.AgentManager1")

    # Clean stale registrations before claiming endpoint
    try:
        manager.UnregisterAgent(AGENT_PATH)
    except dbus.exceptions.DBusException:
        pass

    manager.RegisterAgent(AGENT_PATH, "NoInputNoOutput")
    manager.RequestDefaultAgent(AGENT_PATH)
    print(f"[Agent] Registered {AGENT_PATH} (NoInputNoOutput)", flush=True)

    # Attach signal handlers for systemd stop/restart and Ctrl+C
    signal.signal(signal.SIGINT, cleanup)
    signal.signal(signal.SIGTERM, cleanup)

    loop = GLib.MainLoop()
    try:
        loop.run()
    except KeyboardInterrupt:
        cleanup(signal.SIGINT, None)
