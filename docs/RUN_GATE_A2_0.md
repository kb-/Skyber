# A2-0 Wi-Fi runbook

Use two physical Windows PCs on the same trusted Wi-Fi LAN. On the second
PC, attach Ubuntu 26.04.1 LTS x86-64 VirtualBox adapter 1 in Bridged mode to the
active Wi-Fi adapter. Read the guest address with `ip -4 -br address` and
verify the route with `ip route get WINDOWS_IP`. On Windows, verify the
Wi-Fi address and Private network profile. If the guest lacks a LAN
address or cannot reach the development PC, check DHCP, the selected
adapter, and client isolation on the access point. The Kyber handshake
is the UDP reachability check.

## Build and test locally

Build Windows using the existing README commands. On Ubuntu, install GCC
13, GNU Make, NASM, pkg-config, Git, curl, binutils, Python 3 and OpenSSL
from Ubuntu's package repositories. Install Rust via rustup and use the
repository's pinned toolchain.

~~~bash
bash scripts/build-a2-linux.sh ~/.cache/skyber-a2-0.29.0
python3 scripts/test-a2.py local target/a2/linux/skyber-a1-peer
~~~

The Ubuntu recipe builds pristine FFmpeg n8.1 with pinned x264 and Opus
from the A1 source list. It uses the same C shim and codec identifiers as
Windows. See the [SDK audit](research/KYBER_SDK_AUDIT.md) for licensing.

## Bootstrap guest access

From the Ubuntu console, install OpenSSH Server, create the non-admin
`skyber-a2` account, and provision the build prerequisites. On the
development PC generate a dedicated Ed25519 key outside the checkout.
Install its public key in the guest account's `~/.ssh/authorized_keys`
with directory mode 0700 and file mode 0600. Add an sshd configuration
snippet for `Match User skyber-a2` with `AuthenticationMethods publickey`,
`AllowTcpForwarding no` and `X11Forwarding no`. Validate the effective
configuration with `sudo sshd -t` before reloading. From the guest console
display the SSH host fingerprint with
`ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`. Verify it on first
connection and record it in a dedicated known-hosts file. Set
`StrictHostKeyChecking yes`, `IdentitiesOnly yes`, `IdentityFile`, and
`UserKnownHostsFile` in the Windows SSH host profile. Use `ssh skyber-a2 id`
and SCP a disposable file in both directions to verify access.

For graphical checks, log into the guest as `skyber-a2` and open Settings,
System, Remote Desktop, Desktop Sharing. Enable Desktop Sharing and
Remote Control and read the advertised TCP port. Scope the Ubuntu firewall
rules for TCP 22 and that RDP port to the Windows development PC's Wi-Fi
address. Use the existing firewall manager. For UFW, the rule form is
`sudo ufw allow from WINDOWS_IP to any port PORT proto tcp`. Confirm
the guest has an active inbound firewall policy and keep the console open
while validating it.

Connect with `mstsc /v:UBUNTU_IP:RDP_PORT`, verify the certificate fingerprint
against GNOME's Verify Encryption dialog, and enter the desktop-sharing
credential in the client. Disable clipboard and drive redirection.
For agent desktop operation, install Microsoft WinApp CLI 0.8.0 separately
from the project. Export `WINAPP_CLI_TELEMETRY_OPTOUT=1` before using it.
In an unlocked Windows session, identify the connected window with
`winapp ui list-windows -a mstsc`. Target its handle with
`winapp ui screenshot -w HWND -o target/a2/desktop.png`. Use fresh client
bounds and focus for each `winapp ui click -w HWND --at X,Y` and
`winapp ui send-keys KEYS -w HWND --via send-input`. Verify a click and
keystroke visibly change a disposable guest window. Store desktop images
outside the test report and close RDP before the media run.

Keep management access on the private LAN. To revoke it, turn off Desktop
Sharing, remove the dedicated public key, and delete the two scoped guest
firewall rules. Keep credentials and addresses in local configuration.

## Run and collect results

On Windows, add a temporary Private-profile firewall rule for the chosen
UDP port, scoped to the guest Wi-Fi-side IP. For example, from an
administrator PowerShell set `$guestIp` to the guest address and
`$windowsIp` to the development PC Wi-Fi address:

~~~powershell
New-NetFirewallRule -DisplayName 'Skyber A2-0 UDP' -Direction Inbound -Action Allow -Profile Private -Protocol UDP -LocalPort 34829 -RemoteAddress $guestIp
py -3 scripts/test-a2.py prepare --bind-address $windowsIp --port 34829
~~~

Save the printed run ID as `$runId` and the private staging directory as
`$stage`. Keep the
listener half on Windows and transfer the initiator half with SCP, for
example `scp -r "$stage/initiator" skyber-a2:~/skyber-a2-private/` after
creating that directory with mode 0700. Use the same port and binaries
throughout this ordered matrix.

1. `av-listener`
2. `av-initiator`
3. `concurrent-screen-stop`
4. `wrong-token`
5. `wrong-fingerprint`
6. `rejoin-same-port`

For each case, run the listener first:

~~~powershell
py -3 scripts/test-a2.py run target/a1/skyber-a1-peer.exe "$stage/listener/$case"
~~~

When it prints `listening`, start the matching case on Ubuntu within 60
seconds. Run the command from the Ubuntu checkout via the guest shell or SSH.

~~~bash
python3 scripts/test-a2.py run target/a2/linux/skyber-a1-peer "$HOME/skyber-a2-private/initiator/$case"
~~~

Wait for both processes before moving to the next case. The wrong-fingerprint
listener may time out while waiting for a successful QUIC handshake.
Copy the Ubuntu run directory's per-case `initiator.*` files into the
matching Windows run directory. With the guest checkout at `~/Skyber`,
the following example collects a case and then verifies the complete run
once all six cases have been collected:

~~~powershell
scp "skyber-a2:~/Skyber/target/a2/runs/$runId/$case/initiator.*" "target/a2/runs/$runId/$case/"
py -3 scripts/test-a2.py verify "target/a2/runs/$runId"
~~~

Record Windows and Ubuntu versions, VirtualBox version and bridge settings,
and Wi-Fi topology with the summary. A blocked bridge or failed UDP
handshake leaves A2-0 pending. Remove the test firewall rule with
`Remove-NetFirewallRule -DisplayName 'Skyber A2-0 UDP'` and remove the
disposable credential directories when finished.
