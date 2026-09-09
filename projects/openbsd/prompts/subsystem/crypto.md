# OpenBSD Crypto Framework

Review guidance for sys/crypto and the in-kernel crypto framework (the software
crypto driver swcr, used by IPsec and others) and primitive implementations.

## Correctness
- Constant-time behaviour matters for secret-dependent code: flag branches or
  table lookups indexed by secret data, and early-outs (e.g. a non-constant-time
  memcmp) on MACs/tags. Tag comparison must be constant time.
- Verify key, IV/nonce, and tag lengths against the algorithm; never reuse a
  nonce with the same key for stream ciphers / AEAD.
- Check buffer lengths and alignment before processing blocks; partial-block and
  zero-length handling are common bug sources.

## Integration
- crypto sessions and operations (`crypto_newsession`/`crypto_freesession`,
  crypto descriptors) must be balanced; a leaked session is a resource leak.
- Data may arrive in mbufs or uio; ensure the code handles non-contiguous
  buffers correctly and does not read past the supplied length.
- Clear key material and sensitive buffers (explicit_bzero) before freeing.
- Watch for integer overflow in length arithmetic before allocation or copy.
