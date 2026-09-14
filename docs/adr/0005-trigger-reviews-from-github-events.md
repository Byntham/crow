# Trigger reviews from GitHub events

Crow starts automatic reviews in response to GitHub pull request events rather than periodically scanning repositories for changed PRs. This preserves event-triggered behavior at the cost of requiring an event-delivery mechanism for the persistent review service. Each installation's GitHub App and connection service deliver those events, as described in ADR 0007, which supersedes the earlier shared-service model. API calls to retrieve PR details or publish reviews are compatible with this decision.

One-time startup, recovery, and manually requested catch-up checks repair missed work. Catch-up is enabled by default and toggleable, preserves initial-backlog exclusions, and holds large recovery batches for operator action. This allows recovery without scheduled PR-discovery scans.
