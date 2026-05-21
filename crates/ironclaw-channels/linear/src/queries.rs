//! GraphQL queries for the Linear API.
//!
//! Kept as `const &str` so the client never re-allocates them at call time.

/// `commentCreate` mutation — creates a comment on an issue and (optionally)
/// replies to a parent comment when `input.parentId` is set.
pub const CREATE_COMMENT: &str = r"
mutation CommentCreate($input: CommentCreateInput!) {
  commentCreate(input: $input) { success comment { id } }
}
";

/// `commentUpdate` mutation — edits an existing comment by id.
pub const UPDATE_COMMENT: &str = r"
mutation CommentUpdate($id: String!, $input: CommentUpdateInput!) {
  commentUpdate(id: $id, input: $input) { success comment { id } }
}
";

/// `reactionCreate` mutation — adds an emoji reaction to a comment.
pub const CREATE_REACTION: &str = r"
mutation ReactionCreate($input: ReactionCreateInput!) {
  reactionCreate(input: $input) { success reaction { id } }
}
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_comment_mentions_mutation_and_field() {
        assert!(CREATE_COMMENT.contains("CommentCreate"));
        assert!(CREATE_COMMENT.contains("commentCreate"));
        assert!(CREATE_COMMENT.contains("CommentCreateInput"));
    }

    #[test]
    fn update_comment_mentions_mutation_and_field() {
        assert!(UPDATE_COMMENT.contains("CommentUpdate"));
        assert!(UPDATE_COMMENT.contains("commentUpdate"));
        assert!(UPDATE_COMMENT.contains("CommentUpdateInput"));
    }

    #[test]
    fn create_reaction_mentions_mutation_and_field() {
        assert!(CREATE_REACTION.contains("ReactionCreate"));
        assert!(CREATE_REACTION.contains("reactionCreate"));
        assert!(CREATE_REACTION.contains("ReactionCreateInput"));
    }
}
