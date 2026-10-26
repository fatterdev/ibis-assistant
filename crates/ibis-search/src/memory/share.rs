//! Memory sharing: promotes an actor-owned personal
//! memory to team visibility. Sharing only ever promotes to team — the MCP
//! contract accepts either actor, or the legacy visibility-and-actor pair.

use crate::error::{Result, SearchError};
use crate::search::SearchService;

impl SearchService {
    /// Promotes an actor-owned personal memory to team visibility
    ///
    pub async fn share_memory(
        &self,
        actor_ctx: &crate::ActorContext,
        id: &str,
        explicit_actor: &str,
    ) -> Result<()> {
        let db = self.require_db()?;
        if !ibis_db::ids::is_safe_record_id(id) {
            return Err(SearchError::other(format!("invalid memory id: {id}")));
        }
        let memory = self.memory_for_governance(id).await?;
        let actor = actor_ctx.resolve_trusted_actor(explicit_actor);
        if memory.owner != actor {
            return Err(SearchError::other(format!(
                "memory {id} may be shared only by owner {:?}",
                memory.owner
            )));
        }
        if memory.visibility == "team" {
            return Ok(());
        }
        let sql = format!("UPDATE {id} SET visibility = 'team', updated_at = time::now();");
        db.execute(&sql)
            .await
            .map_err(|e| SearchError::other(format!("share memory: {e}")))?;
        Ok(())
    }

    /// Preserves the MCP-facing contract while sharing only to team. Accepts
    /// either actor, or the legacy visibility-and-actor pair
    ///
    pub async fn memory_share(
        &self,
        actor_ctx: &crate::ActorContext,
        id: &str,
        args: &[String],
    ) -> Result<()> {
        let actor = match args.len() {
            0 => "",
            1 => args[0].as_str(),
            _ => {
                if !args[0].is_empty() && args[0] != "team" {
                    return Err(SearchError::other(
                        "memory sharing only promotes visibility to team",
                    ));
                }
                args[1].as_str()
            }
        };
        self.share_memory(actor_ctx, id, actor).await
    }
}
