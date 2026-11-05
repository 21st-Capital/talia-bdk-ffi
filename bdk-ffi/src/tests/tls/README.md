# Test TLS keys

`server-cert.pem` (a self-signed certificate for localhost and 127.0.0.1), its key
`server-key.pem` and the unrelated key `other-key.pem` are throwaway keys made for the local TLS
server in `../electrum_tls.rs`. They are compiled into the unit tests only, never into the
library, and protect nothing. A secret scanner may report the two private keys; they are safe
to allowlist.
