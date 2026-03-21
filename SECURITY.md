# Security Policy

## Threat Model and Intended Use

voicemcu is designed for **trusted environments** such as home LANs or private VPNs (WireGuard, Tailscale). The security model assumes:

- Room codes are the only access control mechanism
- Users trust other participants in the same room
- The network environment is trusted, or TLS certificate pinning is used

**Public internet exposure is possible but not the primary use case.** See the [Security and deployment](README.md#security-and-deployment) section in the README for hardening guidance.

## Known Limitations

- No user authentication system
- No persistent ban lists
- Per-event broadcast overhead (fresh QUIC stream per signal)
- Ephemeral TLS certificates by default (use `--cert-file`/`--key-file` for persistence)

These are design trade-offs, not security bugs. See [Known limitations](README.md#known-limitations) for the complete list.

## Supported Versions

| Version | Supported          | Notes |
| ------- | ------------------ | ----- |
| 0.1.x   | :white_check_mark: | Current development version |
| < 0.1   | :x:                | Pre-release, not supported |

Security patches will be released for the current 0.x series. Breaking changes may occur between minor versions until 1.0.

## Reporting a Vulnerability

**Please do not open public issues for security vulnerabilities.**

To report a security issue:

1. **Email**: [Your email address or create a security@yourdomain email]
2. **Expected response**: Initial acknowledgment within 72 hours
3. **Disclosure timeline**: Coordinated disclosure after a fix is available, typically 30-90 days depending on severity

### What to include:

- Description of the vulnerability and its impact
- Steps to reproduce (or proof-of-concept if applicable)
- Affected versions
- Any suggested mitigations or fixes

### What qualifies as a security issue:

- Authentication/authorization bypass
- Remote code execution
- Denial of service that bypasses rate limiting
- TLS/certificate verification bypasses
- Memory safety issues (buffer overflows, use-after-free, etc.)

### What does NOT qualify:

- Lack of authentication (this is a known design limitation)
- Issues requiring physical access or control of the host machine
- Social engineering attacks against room codes
- Missing security features that are documented as out-of-scope

## Security Best Practices

If you're deploying voicemcu, follow the guidance in [Security and deployment](README.md#security-and-deployment):

- Use `--cert-hash` for TLS pinning (never `--danger-skip-verify` on untrusted networks)
- Generate hard-to-guess room codes if exposed to the internet
- Run the server as an unprivileged user
- Use firewall rules to restrict inbound traffic
- Only forward ports when actively using the server
- Keep the host OS and Rust toolchain updated

## Acknowledgments

Security researchers who responsibly disclose vulnerabilities will be credited here (with permission).
