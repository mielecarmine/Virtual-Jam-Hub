# JAM-7 Development Environment

Created on November 16, 2026 in AWS project `984263476689`.

| Resource | Value |
| --- | --- |
| Region / AZ | `eu-north-1` / `eu-north-1a` |
| Instance | `i-037113c20a85fa4f0` — `virtual-jam-server` |
| Type / CPU credits | `t3.micro` / standard |
| OS | Ubuntu 24.04 x86_64 |
| Canonical AMI | `ami-084df9e6ad4622caf` |
| Initial endpoint | `16.170.231.226:7777` UDP |
| VPC | `vpc-0301f662621e9cada` (existing default) |
| Subnet | `subnet-0b64e3d8bbd38bdc6` |
| Dedicated security group | `sg-0510529c20c033084` |
| Root volume | `vol-0f10684f78fbdfb7c`, gp3 12 GiB, encrypted, DeleteOnTermination |
| Role / instance profile | `virtual-jam-server-ssm` |

The security group allows inbound traffic only on UDP 7777 from `0.0.0.0/0`.
Outbound traffic is allowed for updates, UDP replies, and SSM management.
The role trusts `ec2.amazonaws.com` and attaches
`arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore`.
The profile is attached to the instance; the SSM agent was verified Online.
No SSH keys or inbound TCP rules were created.

Initial bootstrap via cloud-init: installs `build-essential`, `curl`, and UFW,
configures firewall with deny incoming / allow outgoing, opens `7777/udp`, and enables it.
Deployment via SSM transfers the server sources, installs Rust 1.85.1 with the
minimal profile for user `ubuntu`, builds with
`cargo build --locked --release -p jam-server`, and runs `deploy/install.sh`.
The build checkout lives in `/opt/virtual-jam-src`.

Administrative access via the AWS Systems Manager console or, with an authenticated
CLI and the Session Manager plugin installed:

```sh
aws ssm start-session --region eu-north-1 --profile virtual-jam \
  --target i-037113c20a85fa4f0
```

The application service does not use any AWS SDK or credentials. To audit the role,
verify trust relationships, attached policies, and instance profile from the IAM
console; there is no need to print credentials from the metadata service.

For cleanup, use the names and IDs in this table following `deploy/README.md`.
Keep the pre-existing VPC, subnet, and Internet Gateway.

## Verification Evidence

- Native release build with Rust 1.85.1 completed on the VM.
- `systemd-analyze verify` completed without errors.
- Service `active (running)`, user `jamserver`, `Restart=always`, 2 s interval.
- UFW active and socket listening on `0.0.0.0:7777` UDP.
- Re-installation succeeded on the already running service.
- SIGKILL crash test: PID changed from 4753 to 4783, `NRestarts=1`, service active.
- Probe from local machine to public IP: `JoinAccepted`, peer 1, 48,000 Hz;
  `Pong` with `client_timestamp_us=42` matching the Ping.
- EC2 reboot: service restarted automatically and public probe succeeded again.
- Local gates: fmt, workspace and all-targets Clippy, and workspace tests passed;
  release gate with 5 peers / 500 frames: max 59.792 µs (limit 200 µs).

SHA-256 of the remote binary:
`238d7adc11e9c10c09f40656cce9090749ebc4ee8b071444e460299672061b79`.

The VM is kept running for upcoming tickets at the user's request.
