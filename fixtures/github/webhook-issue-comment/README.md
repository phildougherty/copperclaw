## github / webhook-issue-comment

A GitHub `issue_comment.created` webhook for `octocat/hello#7` lands on
the host. The harness injects a single `InboundEvent` with
`channel_type=github` and `platform_id="octocat/hello#7"`, matching the
shape `copperclaw-channels-github`'s events router emits. Claude responds
with one plain-text turn; the runner emits one outbound chat row; the
github `MockAdapter` records one delivery (i.e. a new issue comment).
