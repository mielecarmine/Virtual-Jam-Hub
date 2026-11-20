# Linux Deployment — JAM-7

The server listens on UDP `0.0.0.0:7777`. The current protocol does not provide
room authentication or encryption: the public endpoint is intended for development testing.

## Installation

On Ubuntu/Debian with systemd and UFW, build with Rust >= 1.85 on the target
machine (or with a toolchain matching the target's architecture and glibc):

```sh
cargo build --locked --release -p jam-server
sudo apt-get install ufw
sudo sh deploy/install.sh target/release/jam-server
```

The installer creates a `jamserver` user without a home directory and with `/bin/false`
as its shell, installs a root-owned binary, enables startup at boot, and restarts the
service. It can be re-run to upgrade the binary. Limits are 128 MiB of memory, 16 tasks,
and 1,024 open files; no CPU quota is imposed on the UDP path.
`Restart=always` and `RestartSec=2` handle crashes and process exits.
An explicit stop with `systemctl stop` intentionally keeps the service stopped.

The installer adds `ufw allow 7777/udp` but does not enable UFW on existing hosts:
check `sudo ufw status` and ensure administrative access is configured first.
Also open UDP 7777 in the cloud security group. Application traffic requires no
TCP ports. On a fresh VM managed via SSM, UFW can be enabled with default deny
incoming and allow outgoing, without SSH ingress rules.

```sh
sudo systemctl status jam-server --no-pager
sudo journalctl -u jam-server -n 50 --no-pager
sudo systemctl show jam-server -p User -p Restart -p RestartUSec -p MemoryMax
sudo ss -lunp 'sport = :7777'
```

## Verification from a Home Network

From a local checkout, replace `PUBLIC_IP` with the VM's address:

```sh
cargo run --locked -p jam-server --example probe -- PUBLIC_IP:7777
```

The probe waits for `JoinAccepted` with the expected sample rate, sends a `Ping`,
verifies the `Pong` timestamp, and sends `Leave`. It retries every 500 ms for
about 3 seconds and exits with an error if the server does not respond. It occupies
one room slot for the duration of the check; if 8 peers are already connected, it
cannot join. Testing over loopback does not verify cloud firewall rules.

To test crash resilience on a development VM with no musicians connected:

```sh
sudo systemctl kill --signal=SIGKILL --kill-whom=main jam-server
sleep 3
sudo systemctl is-active jam-server
sudo systemctl is-enabled jam-server
```

After a reboot, re-check the service status and run the external probe again.

## AWS Resources

Region: `eu-north-1`. Administrative access via Systems Manager; the
`virtual-jam-server-ssm` role assumes EC2 identity with only the
`AmazonSSMManagedInstanceCore` policy. No static credentials are stored on the
service. IMDSv2 is enforced and the gp3 volume is encrypted. No NAT Gateway,
load balancer, or Elastic IP is needed for this verification.

The auto-assigned public IP may change after stop/start.
The VM, disk, public IPv4, and network traffic may consume credits or incur costs:
check AWS Settings > Billing. Stopping the VM reduces compute usage while keeping
the disk. To clean up everything after testing: terminate the instance (the root
volume has `DeleteOnTermination`), delete the dedicated security group, remove the
role from the instance profile, delete the instance profile, detach
`AmazonSSMManagedInstanceCore`, and finally delete the dedicated role.
