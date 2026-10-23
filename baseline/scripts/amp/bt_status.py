#!/usr/bin/env python3
import subprocess
import shutil
import sys
import re

def run_cmd(cmd):
    try:
        res = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, check=False)
        return res.stdout.strip(), res.stderr.strip(), res.returncode
    except FileNotFoundError:
        return "", f"Command '{cmd[0]}' not found.", 127

def check_service(name):
    stdout, _, _ = run_cmd(["systemctl", "is-active", name])
    return stdout == "active"

def get_adapter_info():
    stdout, _, code = run_cmd(["bluetoothctl", "show"])
    if code != 0 or not stdout:
        return None
    
    info = {"name": "Unknown", "addr": "Unknown", "powered": "no", "discoverable": "no", "pairable": "no"}
    for line in stdout.splitlines():
        line = line.strip()
        if line.startswith("Controller"):
            parts = line.split()
            if len(parts) >= 2:
                info["addr"] = parts[1]
        elif line.startswith("Name:"):
            info["name"] = line.split(":", 1)[1].strip()
        elif line.startswith("Powered:"):
            info["powered"] = line.split(":", 1)[1].strip()
        elif line.startswith("Discoverable:"):
            info["discoverable"] = line.split(":", 1)[1].strip()
        elif line.startswith("Pairable:"):
            info["pairable"] = line.split(":", 1)[1].strip()
    return info

def get_connected_devices():
    stdout, _, code = run_cmd(["bluetoothctl", "devices", "Connected"])
    devices = {}
    if code == 0 and stdout:
        for line in stdout.splitlines():
            parts = line.split(maxsplit=2)
            if len(parts) >= 3 and parts[0] == "Device":
                devices[parts[1]] = parts[2]
    return devices

def get_bluealsa_pcms(connected_map):
    if not shutil.which("bluealsa-cli"):
        return None

    stdout, _, code = run_cmd(["bluealsa-cli", "list-pcms"])
    if code != 0 or not stdout:
        return []

    pcm_paths = [line.strip() for line in stdout.splitlines() if line.strip().startswith("/org/bluealsa/")]

    pcms = []
    for path in pcm_paths:
        info_out, _, info_code = run_cmd(["bluealsa-cli", "info", path])
        if info_code != 0:
            continue

        props = {}
        for line in info_out.splitlines():
            if ":" in line:
                k, v = line.split(":", 1)
                props[k.strip().lower()] = v.strip()

        dev_match = re.search(r"dev_([0-9A-Fa-f]{2}_[0-9A-Fa-f]{2}_[0-9A-Fa-f]{2}_[0-9A-Fa-f]{2}_[0-9A-Fa-f]{2}_[0-9A-Fa-f]{2})", path)
        mac_addr = dev_match.group(1).replace("_", ":") if dev_match else "Unknown"
        dev_name = connected_map.get(mac_addr, "Unknown Device")

        props["mac"] = mac_addr
        props["resolved_name"] = dev_name
        pcms.append(props)

    return pcms

def main():
    print("=========================================================")
    print("               BLUETOOTH & BLUE-ALSA STATUS              ")
    print("=========================================================")

    bt_service = check_service("bluetooth")
    ba_service = check_service("bluealsa") or check_service("bluealsa-aplay")

    print(f"Bluetooth Service (bluetoothd) : {'Running' if bt_service else 'INACTIVE / STOPPED'}")
    print(f"BlueALSA Service (bluealsa)    : {'Running' if ba_service else 'INACTIVE / STOPPED'}")
    print("---------------------------------------------------------")

    adapter = get_adapter_info()
    if not adapter:
        print("[CRITICAL] No Bluetooth controller detected.")
        print("=========================================================")
        sys.exit(1)

    print(f"Controller Name       : {adapter['name']} ({adapter['addr']})")
    print(f"Radio Power State     : {'ON' if adapter['powered'] == 'yes' else 'OFF'}")
    print(f"Discoverable / Pair   : {adapter['discoverable'].upper()} / {adapter['pairable'].upper()}")
    print("---------------------------------------------------------")

    connected_map = get_connected_devices()
    print(f"Connected Devices ({len(connected_map)}):")
    if not connected_map:
        print("  [IDLE] No active Bluetooth connections")
    else:
        for mac, name in connected_map.items():
            print(f"  [CONNECTED] {name} ({mac})")

    print("---------------------------------------------------------")
    print("BlueALSA Audio Pipeline:")
    pcms = get_bluealsa_pcms(connected_map)

    if pcms is None:
        print("  [NOTICE] 'bluealsa-cli' tool not found.")
    elif not pcms:
        print("  [STANDBY] No active audio transport streams registered.")
    else:
        for pcm in pcms:
            running = pcm.get("running", "false").lower() == "true"
            stream_state = "STREAMING (Active Audio Flow)" if running else "IDLE / PAUSED (Stream Established)"

            codec = pcm.get("selected codec", "Unknown")
            codecs_avail = pcm.get("available codecs", "N/A")
            sampling = pcm.get("sampling", "Unknown")
            fmt = pcm.get("format", "Unknown")
            channels = pcm.get("channels", "2")
            transport = pcm.get("transport", "A2DP")
            delay = pcm.get("delay", "N/A")
            volume = pcm.get("volume", "N/A")
            muted = pcm.get("muted", "N/A")

            print(f"  Stream State      : {stream_state}")
            print(f"  ↳ Audio Source    : {pcm['resolved_name']} ({pcm['mac']})")
            print(f"  ↳ Transport Mode  : {transport}")
            print(f"  ↳ Active Codec    : {codec}")
            print(f"  ↳ Sample Rate     : {sampling} ({channels} Channels, {fmt})")
            print(f"  ↳ Hardware Volume : {volume} (Muted: {muted})")
            print(f"  ↳ Audio Latency   : {delay}")
            print(f"  ↳ Codecs Allowed  : {codecs_avail}")
            print("")

    print("=========================================================")

if __name__ == "__main__":
    main()
