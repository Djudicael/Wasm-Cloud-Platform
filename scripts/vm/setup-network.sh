#!/usr/bin/env bash
# Set up host networking for the microVM testbed.
#
# This script creates the bridge and TAP devices needed for microVMs.
# Run with sudo.
#
# Usage:
#   sudo ./scripts/vm/setup-network.sh

set -euo pipefail

BRIDGE_NAME="${BRIDGE_NAME:-br-wasm}"
BRIDGE_IP="${BRIDGE_IP:-172.20.0.1/24}"
SUBNET="${SUBNET:-172.20.0.0/24}"

echo "=== Setting up testbed network ==="

# Create bridge if it doesn't exist
if ! ip link show "$BRIDGE_NAME" &>/dev/null; then
    echo "Creating bridge $BRIDGE_NAME..."
    ip link add "$BRIDGE_NAME" type bridge
    ip addr add "$BRIDGE_IP" dev "$BRIDGE_NAME"
    ip link set "$BRIDGE_NAME" up
    echo "✅ Bridge created"
else
    echo "Bridge $BRIDGE_NAME already exists"
fi

# Enable IP forwarding
if [[ "$(cat /proc/sys/net/ipv4/ip_forward)" != "1" ]]; then
    echo "Enabling IP forwarding..."
    sysctl -w net.ipv4.ip_forward=1
    echo "✅ IP forwarding enabled"
fi

# Enable NAT if not already enabled
if ! iptables -t nat -C POSTROUTING -s "$SUBNET" ! -o "$BRIDGE_NAME" -j MASQUERADE &>/dev/null; then
    echo "Enabling NAT for $SUBNET..."
    iptables -t nat -A POSTROUTING -s "$SUBNET" ! -o "$BRIDGE_NAME" -j MASQUERADE
    iptables -A FORWARD -i "$BRIDGE_NAME" -j ACCEPT
    iptables -A FORWARD -o "$BRIDGE_NAME" -j ACCEPT
    echo "✅ NAT enabled"
else
    echo "NAT already enabled"
fi

echo ""
echo "=== Network setup complete ==="
echo "Bridge: $BRIDGE_NAME ($BRIDGE_IP)"
echo "Subnet: $SUBNET"
ip addr show "$BRIDGE_NAME"
