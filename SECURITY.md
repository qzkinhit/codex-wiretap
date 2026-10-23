# Security and local data

Pool Router binds only to a loopback address. Management endpoints and the pool-backed model path require the generated local token. Host and Origin checks reject cross-site access.

When legacy monitoring is enabled, the two Responses POST endpoints may accept a caller-provided `sk-` credential. This path forwards only that credential to the fixed AnyRouter origin. It cannot select stored pool credentials or access management endpoints and the model list.

BiWrite credentials are read into memory. Manually added credentials are stored in `manual-keys.json` in the private application data directory. Configuration and recovery backups can contain credentials. These files are private local state and must not be included in bug reports or commits. The application uses owner-only file permissions on Unix systems.

Monitoring keeps an allowlist of metadata rather than prompt, response, tool, or authorization bodies. Model, reasoning and service-tier values are upstream declarations, not independent verification of execution or billing. Published screenshots use synthetic data.

Do not put credentials in public GitHub issues. Use GitHub private vulnerability reporting if it is enabled for this repository; otherwise contact the repository owner without including a working credential or private conversation.
