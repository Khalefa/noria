use std::future::Future;
use trawler::{StoryId, UserId, Vote};

pub(crate) async fn handle<F>(
    c: F,
    acting_as: Option<UserId>,
    comment: StoryId,
    v: Vote,
) -> Result<(crate::Conn, bool), failure::Error>
where
    F: 'static + Future<Output = Result<crate::Conn, failure::Error>> + Send,
{
    let c = c.await?;
    let user = acting_as.unwrap();

    let mut comment = c
        .view("comment_vote_1")
        .await?
        .lookup_first(&[::std::str::from_utf8(&comment[..]).unwrap().into()], true)
        .await?
        .unwrap();

    let sid = comment.take("story_id").unwrap();
    let comment = comment.take("id").unwrap();
    let _ = c
        .view("comment_vote_2")
        .await?
        .lookup(&[user.into(), sid.clone(), comment.clone()], true)
        .await?;

    // TODO: do something else if user has already voted
    // TODO: technically need to re-load comment under transaction

    // NOTE: MySQL technically does everything inside this and_then in a transaction,
    // but let's be nice to it
    c.table("vote")
        .await?
        .insert(vec![
            user.into(),
            sid,
            comment,
            match v {
                Vote::Up => 1,
                Vote::Down => 0,
            }
            .into(),
        ])
        .await?;

    Ok((c, false))
}
