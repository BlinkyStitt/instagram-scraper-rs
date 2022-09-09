//! # Session
//!
//! This module exposes the session for the instagram client

use crate::{Authentication, InstagramScraperError, InstagramScraperResult, Post};

use reqwest::{header, Client, ClientBuilder};

mod requests;
use requests::{BASE_URL, LOGIN_URL, LOGOUT_URL, STORIES_USER_AGENT, X_CSRF_TOKEN};

pub use crate::{Stories, Story, User};

/// The session is a storage for values required by the instagram client to work.
/// It also exposes the instagram HTTP client
#[derive(Debug)]
pub struct Session {
    csrftoken: Option<String>,
    client: Client,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            csrftoken: None,
            client: ClientBuilder::new()
                .cookie_store(true)
                .user_agent(STORIES_USER_AGENT)
                .build()
                .unwrap(),
        }
    }
}

impl Session {
    /// Login into instagram account or as a guest based on provided authentication type
    pub(crate) async fn login(
        &mut self,
        authentication: Authentication,
    ) -> InstagramScraperResult<()> {
        let token = match authentication {
            Authentication::Guest => self.login_as_guest().await?,
            Authentication::UsernamePassword { username, password } => {
                self.login_as_user(username, password).await?
            }
        };
        debug!("login successful; csrf token: {}", token);
        self.csrftoken = Some(token);
        Ok(())
    }

    /// Scrape profile picture for provided username.
    ///
    /// Returns the image url
    pub async fn scrape_profile_pic(
        &mut self,
        user_id: &str,
    ) -> InstagramScraperResult<Option<String>> {
        self.restrict_authed()?;
        debug!("collecting profile pic for {}", user_id);
        let response = self
            .client
            .get(format!(
                "https://i.instagram.com/api/v1/users/{}/info/",
                user_id.to_string()
            ))
            .send()
            .await?;
        Self::restrict_successful(&response)?;
        let user_info = response
            .text()
            .await
            .map(|t| serde_json::from_str::<requests::UserInfoResponse>(&t).map(|u| u.user))?;
        let user_info = user_info?;
        if user_info.has_anonymous_profile_picture.unwrap_or_default() {
            debug!("user has anonymous profile picture");
            return Ok(None);
        }
        if let Some(url) = user_info.hd_profile_pic_url_info.url {
            debug!("found hd profile pic {}", url);
            Ok(Some(url))
        } else {
            debug!("searching user profile pic in versions");
            Ok(user_info
                .hd_profile_pic_versions
                .map(|images| {
                    images
                        .into_iter()
                        .rev()
                        .find(|img| img.url.is_some())
                        .map(|img| img.url.unwrap())
                })
                .flatten())
        }
    }

    /// Scrape shared data for user
    pub async fn scrape_shared_data_userinfo(
        &mut self,
        username: &str,
    ) -> InstagramScraperResult<User> {
        self.restrict_authed()?;
        debug!("collecting user info for {}", username);
        let response = self
            .client
            .get(format!(
                "https://i.instagram.com/api/v1/users/web_profile_info/?username={}",
                username
            ))
            .send()
            .await?;
        Self::restrict_successful(&response)?;
        match response
            .text()
            .await
            .map(|t| serde_json::from_str::<requests::WebProfileResponse>(&t).map(|i| i.data.user))
        {
            Err(err) => Err(err.into()),
            Ok(Ok(user)) => Ok(user),
            Ok(Err(err)) => Err(err.into()),
        }
    }

    /// Scrape user stories
    pub async fn scrape_stories(&mut self, user_id: &str) -> InstagramScraperResult<Stories> {
        self.restrict_authed()?;
        debug!("collecting stories for {}", user_id);
        let main_stories = self.fetch_stories(format!("{}graphql/query/?query_hash=45246d3fe16ccc6577e0bd297a5db1ab&variables=%7B%22reel_ids%22%3A%5B%22{}%22%5D%2C%22tag_names%22%3A%5B%5D%2C%22location_ids%22%3A%5B%5D%2C%22highlight_reel_ids%22%3A%5B%5D%2C%22precomposed_overlay%22%3Afalse%7D", BASE_URL, user_id))
            .await?;
        debug!("collected main stories; collecting highlight stories");
        // fetch highlight stories
        let highlight_stories_ids = self.fetch_highlighted_stories_ids(user_id).await?;
        debug!(
            "found {} ids for highlighted stories",
            highlight_stories_ids.len()
        );
        let mut highlight_stories = Vec::with_capacity(highlight_stories_ids.len());
        for chunk in highlight_stories_ids.chunks(3) {
            let id = chunk.join("%22%2C%22");
            debug!("fetching stories in chunk {}", id);
            highlight_stories.extend(
                self.fetch_stories(format!("{}graphql/query/?query_hash=45246d3fe16ccc6577e0bd297a5db1ab&variables=%7B%22reel_ids%22%3A%5B%5D%2C%22tag_names%22%3A%5B%5D%2C%22location_ids%22%3A%5B%5D%2C%22highlight_reel_ids%22%3A%5B%22{}%22%5D%2C%22precomposed_overlay%22%3Afalse%7D", BASE_URL, id)).await?
            );
        }

        Ok(Stories {
            main_stories,
            highlight_stories,
        })
    }

    /// Scrape posts published by user associated to `user_id`
    pub async fn scrape_posts(&mut self, user_id: &str) -> InstagramScraperResult<Vec<Post>> {
        self.restrict_authed()?;
        debug!("collecting posts for {}", user_id);
        // TODO: allow max queries / max items
        let mut posts = Vec::new();
        let mut cursor = String::default();
        loop {
            debug!("collecting 50 posts from {}", cursor);
            let params = format!(r#"{{"id":"{}","first":50,"after":"{}"}}"#, user_id, cursor);
            let response = self
                .client
                .get(format!(
                    "{}graphql/query/?query_hash=42323d64886122307be10013ad2dcc44&variables={}",
                    BASE_URL, params
                ))
                .send()
                .await?;
            Self::restrict_successful(&response)?;
            match response
                .text()
                .await
                .map(|t| serde_json::from_str::<requests::PostResponse>(&t))
            {
                Err(err) => return Err(err.into()),
                Ok(Ok(post_response)) => {
                    let new_cursor = post_response.end_cursor().to_string();
                    let response_posts = post_response.posts();
                    debug!("found {} posts", response_posts.len());
                    posts.extend(response_posts);
                    debug!(
                        "checking cursor; new cursor: {}; last cursor: {}",
                        new_cursor, cursor
                    );
                    if new_cursor == cursor {
                        debug!("leaving loop");
                        break;
                    }
                    cursor = new_cursor;
                }
                Ok(Err(err)) => return Err(err.into()),
            }
        }
        Ok(posts)
    }

    // -- private

    /// Logout from Instagram
    pub(crate) async fn logout(&mut self) -> InstagramScraperResult<()> {
        if let Some(csrf_token) = self.csrftoken.as_deref() {
            let response = self
                .client
                .post(LOGOUT_URL)
                .form(&requests::LogoutRequest::new(csrf_token.to_string()).to_form())
                .header(header::REFERER, BASE_URL)
                .header(X_CSRF_TOKEN, csrf_token.to_string())
                .header("X-Requested-With", "XMLHttpRequest")
                .send()
                .await?;
            Self::restrict_successful(&response)
        } else {
            error!("unauthenticated user; cannot logout");
            Err(InstagramScraperError::Unauthenticated)
        }
    }

    /// Returns whether session is authed
    pub(crate) fn authed(&self) -> bool {
        self.csrftoken.is_some()
    }

    /// Login to instagram as a guest
    async fn login_as_guest(&self) -> InstagramScraperResult<String> {
        debug!("authenticating as guest");
        self.request_csrftoken().await
    }

    /// Login to instagram as an authenticated user
    async fn login_as_user(
        &mut self,
        username: String,
        password: String,
    ) -> InstagramScraperResult<String> {
        debug!("authenticating with username and password");
        let token = self.request_csrftoken().await?;
        let response = self
            .client
            .post(LOGIN_URL)
            .form(
                requests::UsernamePasswordLoginRequest::new(username, password)
                    .to_form()
                    .as_slice(),
            )
            .header(header::REFERER, BASE_URL)
            .header(header::USER_AGENT, STORIES_USER_AGENT)
            .header(X_CSRF_TOKEN, token.clone())
            .header("X-Requested-With", "XMLHttpRequest")
            .send()
            .await?;
        Self::restrict_successful(&response)?;
        debug!("setting cookies received from response");
        let body: requests::UsernamePasswordLoginResponse = response.json().await?;
        if body.authenticated {
            debug!("user authenticated successfully");
            Ok(token)
        } else {
            error!("login failed: {:?}; {:?}", body.status, body.message);
            Err(InstagramScraperError::AuthenticationFailed {
                status: body.status.unwrap_or_default(),
                message: body.message.unwrap_or_default(),
            })
        }
    }

    async fn request_csrftoken(&self) -> InstagramScraperResult<String> {
        let response = self
            .client
            .get(BASE_URL)
            .header(header::REFERER, BASE_URL)
            .header(header::USER_AGENT, STORIES_USER_AGENT)
            .send()
            .await?;
        Self::restrict_successful(&response)?;
        trace!("login status: {}", response.status());
        let mut cookies = response.cookies();
        match cookies
            .find(|x| x.name() == "csrftoken")
            .map(|x| x.value().to_string())
        {
            Some(cookie) => Ok(cookie),
            None => Err(InstagramScraperError::CsrfTokenIsMissing),
        }
    }

    /// Fetch stories from url
    async fn fetch_stories(&mut self, url: String) -> InstagramScraperResult<Vec<Story>> {
        debug!("fetching user stories at {}", url);
        let response = self.client.get(url).send().await?;
        match response
            .text()
            .await
            .map(|t| serde_json::from_str::<requests::ReelsMedia>(&t).map(|i| i.items()))
        {
            Err(err) => Err(err.into()),
            Ok(Ok(stories)) => Ok(stories.into_iter().map(Story::from).collect()),
            Ok(Err(err)) => Err(err.into()),
        }
    }

    /// Fetch highlighted stories ids
    async fn fetch_highlighted_stories_ids(
        &mut self,
        user_id: &str,
    ) -> InstagramScraperResult<Vec<String>> {
        let response = self.client.get(format!("{}graphql/query/?query_hash=c9100bf9110dd6361671f113dd02e7d6&variables=%7B%22user_id%22%3A%22{}%22%2C%22include_chaining%22%3Afalse%2C%22include_reel%22%3Afalse%2C%22include_suggested_users%22%3Afalse%2C%22include_logged_out_extras%22%3Afalse%2C%22include_highlight_reels%22%3Atrue%2C%22include_related_profiles%22%3Afalse%7D", BASE_URL, user_id)).send().await?;
        match response
            .text()
            .await
            .map(|t| serde_json::from_str::<requests::HighlightReels>(&t).map(|i| i.node_ids()))
        {
            Err(err) => Err(err.into()),
            Ok(Ok(ids)) => Ok(ids),
            Ok(Err(err)) => Err(err.into()),
        }
    }

    /// This function puts a restriction on a function flow to return in case of an unsuccessful status code in the HTTP response.
    ///
    /// it must be called as `Self::restrict_successful(&response)?;`
    fn restrict_successful(response: &reqwest::Response) -> InstagramScraperResult<()> {
        debug!("response status {}", response.status());
        match response.status().is_success() {
            true => Ok(()),
            false => Err(InstagramScraperError::from(response.status())),
        }
    }

    /// This function puts a restriction on a function flow to return in case we're not authenticated
    fn restrict_authed(&self) -> InstagramScraperResult<()> {
        trace!("checking authentication");
        if self.authed() {
            trace!("authed");
            Ok(())
        } else {
            error!("unauthenticated user, but authentication is required");
            Err(InstagramScraperError::Unauthenticated)
        }
    }
}

#[cfg(test)]
mod test {

    use super::*;

    use pretty_assertions::assert_eq;

    #[test]
    fn should_initialize_session() {
        let session = Session::default();
        assert!(session.csrftoken.is_none());
        assert!(!session.authed());
    }

    #[tokio::test]
    async fn should_login_as_guest() {
        let mut session = Session::default();
        assert!(session.login(Authentication::Guest).await.is_ok());
        assert!(session.authed());
    }

    #[cfg(not(feature = "github-ci"))]
    #[tokio::test]
    async fn should_login_as_user() {
        let username =
            std::env::var("INSTAGRAM_USERNAME").expect("missing env key INSTAGRAM_USERNAME");
        let password =
            std::env::var("INSTAGRAM_PASSWORD").expect("missing env key INSTAGRAM_PASSWORD");
        let mut session = Session::default();
        assert!(session
            .login(Authentication::UsernamePassword { username, password })
            .await
            .is_ok());
        assert!(session.authed());
    }

    #[tokio::test]
    async fn should_scrape_user_profile_picture() {
        let mut session = Session::default();
        assert!(session.login(Authentication::Guest).await.is_ok());
        let user_id = session
            .scrape_shared_data_userinfo("bigluca.marketing")
            .await
            .unwrap()
            .id;
        assert!(session
            .scrape_profile_pic(&user_id)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn should_scrape_shared_userinfo() {
        let mut session = Session::default();
        assert!(session.login(Authentication::Guest).await.is_ok());
        assert_eq!(
            session
                .scrape_shared_data_userinfo("bigluca.marketing")
                .await
                .unwrap()
                .username
                .as_str(),
            "bigluca.marketing"
        );
    }

    #[tokio::test]
    async fn should_scrape_user_stories() {
        let mut session = Session::default();
        assert!(session.login(Authentication::Guest).await.is_ok());
        let user_id = session
            .scrape_shared_data_userinfo("tamadogecoin")
            .await
            .unwrap()
            .id;
        assert!(session.scrape_stories(&user_id).await.is_ok());
    }

    #[tokio::test]
    async fn should_scrape_user_posts() {
        let mut session = Session::default();
        assert!(session.login(Authentication::Guest).await.is_ok());
        let user_id = session
            .scrape_shared_data_userinfo("tamadogecoin")
            .await
            .unwrap()
            .id;
        assert!(session.scrape_posts(&user_id).await.is_ok());
    }
}
