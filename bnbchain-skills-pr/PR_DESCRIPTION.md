# feat: add BAL verification and block access list inspection tools

This PR adds two tools to the `bnbchain-skills` MCP server for managing and inspecting Block Access Lists (BAL).

### Tools Added
1. **verify_bal_payload**: Decodes a hex-encoded BAL payload to verify its RLP structure and consistency. Checks Version 0 compliance and validates address and key lengths.
2. **get_block_bal_stats**: Inspects a specific block to check if it contains a BAL payload in its protocol extension. Returns metrics like total accounts, storage slots, and final payload size.

### References
- Fixes bnb-chain/bsc#3596
- Contributed by @Ramprasad4121
