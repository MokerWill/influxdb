use arrow_flight::error::FlightError;
use arrow_util::assert_batches_sorted_eq;
use hashbrown::HashMap;
use influxdb3_client::Precision;
use influxdb3_server::all_paths;
use itertools::Itertools;
use observability_deps::tracing::info;
use reqwest::StatusCode;

use crate::server::{ConfigProvider, TestServer, collect_stream};

#[tokio::test]
async fn auth_http() {
    let server = TestServer::configure().with_auth().spawn().await;
    let token = server
        .auth_token
        .clone()
        .expect("admin token to have been present");

    let client = server.http_client();
    let base = server.client_addr();
    let write_lp_url = format!("{base}/api/v3/write_lp");
    let write_lp_params = [("db", "foo")];
    let query_sql_url = format!("{base}/api/v3/query_sql");
    let query_sql_params = [("db", "foo"), ("q", "select * from cpu")];

    assert_eq!(
        client
            .post(&write_lp_url)
            .query(&write_lp_params)
            .body("cpu,host=a val=1i 2998574937")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(&query_sql_url)
            .query(&query_sql_params)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .post(&write_lp_url)
            .query(&write_lp_params)
            .body("cpu,host=a val=1i 2998574937")
            .bearer_auth(token.clone())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        client
            .post(&write_lp_url)
            .query(&write_lp_params)
            .body("cpu,host=a val=1i 2998574937")
            // support both Bearer and Token auth schemes
            .header("Authorization", format!("Token {token}"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        client
            .get(&query_sql_url)
            .query(&query_sql_params)
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    // Malformed Header Tests
    // Test that there is an extra string after the token foo
    assert_eq!(
        client
            .get(&query_sql_url)
            .query(&query_sql_params)
            .header("Authorization", format!("Bearer {token} whee"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .get(&query_sql_url)
            .query(&query_sql_params)
            .header("Authorization", format!("bearer {token}"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .get(&query_sql_url)
            .query(&query_sql_params)
            .header("Authorization", "Bearer")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .get(&query_sql_url)
            .query(&query_sql_params)
            .header("auth", format!("Bearer {token}"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn http_write_basic_auth() {
    let server = TestServer::configure().with_auth().spawn().await;
    let token = server
        .auth_token
        .clone()
        .expect("admin token to have been present");

    let client = server.http_client();
    let base = server.client_addr();
    let write_lp_url = format!("{base}/write");
    let write_lp_params = [("db", "foo")];
    let write_lp_params_with_user_and_password = [("db", "foo"), ("u", "ignored"), ("p", &token)];
    assert_eq!(
        client
            .post(&write_lp_url)
            .query(&write_lp_params)
            .body("cpu,host=a val=1i 2998574937")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .post(&write_lp_url)
            .query(&write_lp_params)
            .body("cpu,host=a val=1i 2998574937")
            .basic_auth("username", Some(token.clone()))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    // Note: this test does not use Authorization header
    assert_eq!(
        client
            .post(&write_lp_url)
            .query(&write_lp_params_with_user_and_password)
            .body("cpu,host=a val=1i 2998574937")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    // Malformed Header Tests
    let resp = client
        .get(&write_lp_url)
        .query(&write_lp_params)
        .header("Authorization", format!("Basic {token} whee"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.text().await.unwrap(),
        r#"{"error": "Authorization header was malformed, the request was not in the form of 'Authorization: <auth-scheme> <token>', supported auth-schemes are Bearer, Token and Basic"}"#
    )
}

#[test_log::test(tokio::test)]
async fn auth_grpc() {
    let server = TestServer::configure().with_auth().spawn().await;
    let token = server
        .auth_token
        .clone()
        .expect("admin token to have been present");
    // Write some data to the server, this will be authorized through the HTTP API
    server
        .write_lp_to_db(
            "foo",
            "cpu,host=s1,region=us-east usage=0.9 2998574937\n\
            cpu,host=s1,region=us-east usage=0.89 2998574938\n\
            cpu,host=s1,region=us-east usage=0.85 2998574939",
            Precision::Second,
        )
        .await
        .unwrap();

    // Check that with a valid authorization header, it succeeds:
    for header in ["authorization", "Authorization"] {
        // Spin up a FlightSQL client
        let mut client = server.flight_sql_client("foo").await;

        // Set the authorization header on the client:
        client
            .add_header(header, &format!("Bearer {token}"))
            .unwrap();

        // Make the query again, this time it should work:
        let response = client
            .query("SELECT host, region, time, usage FROM cpu")
            .await
            .unwrap();
        let batches = collect_stream(response).await;
        assert_batches_sorted_eq!(
            [
                "+------+---------+----------------------+-------+",
                "| host | region  | time                 | usage |",
                "+------+---------+----------------------+-------+",
                "| s1   | us-east | 2065-01-07T17:28:57Z | 0.9   |",
                "| s1   | us-east | 2065-01-07T17:28:58Z | 0.89  |",
                "| s1   | us-east | 2065-01-07T17:28:59Z | 0.85  |",
                "+------+---------+----------------------+-------+",
            ],
            &batches
        );
    }

    // Check that without providing an Authentication header, it gives back
    // an Unauthenticated error:
    {
        let mut client = server.flight_sql_client("foo").await;
        let error = client.query("SELECT * FROM cpu").await.unwrap_err();
        assert!(matches!(error, FlightError::Tonic(s) if s.code() == tonic::Code::Unauthenticated));
    }

    // Create some new clients that set the authorization header incorrectly to
    // ensure errors are returned:

    // Misspelled "Bearer"
    {
        let mut client = server.flight_sql_client("foo").await;
        client
            .add_header("authorization", &format!("bearer {token}"))
            .unwrap();
        let error = client.query("SELECT * FROM cpu").await.unwrap_err();
        assert!(matches!(error, FlightError::Tonic(s) if s.code() == tonic::Code::Unauthenticated));
    }

    // Invalid token, this actually gives Permission denied
    {
        let mut client = server.flight_sql_client("foo").await;
        client
            .add_header("authorization", "Bearer invalid-token")
            .unwrap();
        let error = client.query("SELECT * FROM cpu").await.unwrap_err();
        assert!(matches!(error, FlightError::Tonic(s) if s.code() == tonic::Code::Unauthenticated));
    }

    // Misspelled header key
    {
        let mut client = server.flight_sql_client("foo").await;
        client
            .add_header("auth", &format!("Bearer {token}"))
            .unwrap();
        let error = client.query("SELECT * FROM cpu").await.unwrap_err();
        assert!(matches!(error, FlightError::Tonic(s) if s.code() == tonic::Code::Unauthenticated));
    }
}

#[tokio::test]
async fn v1_password_parameter() {
    let server = TestServer::configure().with_auth().spawn().await;
    let token = server
        .auth_token
        .clone()
        .expect("admin token to have been present");

    let client = server.http_client();
    let query_url = format!("{base}/query", base = server.client_addr());
    let write_url = format!("{base}/write", base = server.client_addr());
    // Send requests without any authentication:
    assert_eq!(
        client
            .get(&query_url)
            .send()
            .await
            .expect("send request")
            .status(),
        StatusCode::UNAUTHORIZED,
    );
    assert_eq!(
        client
            .get(&write_url)
            .send()
            .await
            .expect("send request")
            .status(),
        StatusCode::UNAUTHORIZED,
    );

    // Ensure that an invalid token passed in the `p` parameter is still unauthorized:
    assert_eq!(
        client
            .get(&query_url)
            .query(&[("p", "not-the-token-you-were-looking-for")])
            .send()
            .await
            .expect("send request")
            .status(),
        StatusCode::UNAUTHORIZED,
    );
    assert_eq!(
        client
            .get(&write_url)
            .query(&[("p", "not-the-token-you-were-looking-for")])
            .send()
            .await
            .expect("send request")
            .status(),
        StatusCode::UNAUTHORIZED,
    );

    // make some writes so that the query API will work below:
    server
        .write_lp_to_db("foo", "cpu,host=a usage=0.9", Precision::Second)
        .await
        .unwrap();

    // Send request to query API with the token in the v1 `p` parameter:
    assert_eq!(
        client
            .get(&query_url)
            .query(&[
                ("p", token.as_str()),
                ("q", "SELECT * FROM cpu"),
                ("db", "foo")
            ])
            .send()
            .await
            .expect("send request")
            .status(),
        StatusCode::OK,
    );
    // Send request to query API with the token in auth header:
    assert_eq!(
        client
            .get(&query_url)
            .query(&[("q", "SELECT * FROM cpu"), ("db", "foo")])
            .bearer_auth(&token)
            .send()
            .await
            .expect("send request")
            .status(),
        StatusCode::OK,
    );

    let valid_write_body = "cpu,host=val usage=0.5";

    // Send request to write API with the token in the v1 `p` parameter:
    assert_eq!(
        client
            .post(&write_url)
            .query(&[("p", token.as_str()), ("db", "foo")])
            .body(valid_write_body)
            .send()
            .await
            .expect("send request")
            .status(),
        StatusCode::NO_CONTENT,
    );
    // Send request to write API with the token in auth header:
    assert_eq!(
        client
            .post(&write_url)
            .bearer_auth(&token)
            .query(&[("db", "foo")])
            .body(valid_write_body)
            .send()
            .await
            .expect("send request")
            .status(),
        StatusCode::NO_CONTENT,
    );
}

#[test_log::test(tokio::test)]
async fn disabled_auth_for_health_and_ping() {
    let server = TestServer::configure()
        .with_auth()
        .with_disable_authz(vec!["health".to_owned(), "ping".to_owned()])
        .spawn()
        .await;

    let client = server.http_client();
    let base = server.client_addr();

    // health v3 API works
    assert_eq!(
        client
            .get(format!("{base}/health"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    // health v1 API works
    assert_eq!(
        client
            .get(format!("{base}/api/v1/health"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    // ping works
    assert_eq!(
        client
            .get(format!("{base}/ping"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    // metrics does not work, it requires token
    assert_eq!(
        client
            .get(format!("{base}/metrics"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );

    let token = server
        .auth_token
        .clone()
        .expect("admin token to have been present");

    // with token metrics endpoint works too
    assert_eq!(
        client
            .get(format!("{base}/metrics"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[test_log::test(tokio::test)]
async fn disabled_auth_test_exhaustive() {
    let allowed_resources = ["health".to_owned(), "ping".to_owned(), "metrics".to_owned()];
    let all_possible_disable_auth_resources =
        allowed_resources.iter().powerset().collect::<Vec<_>>();

    for combo in &all_possible_disable_auth_resources {
        let resources_disabled: Vec<String> = combo.iter().map(|r| r.to_string()).collect();
        let mut all_paths = HashMap::new();
        all_paths.insert(
            "health",
            vec![all_paths::API_V3_HEALTH, all_paths::API_V1_HEALTH],
        );
        all_paths.insert("ping", vec![all_paths::API_PING]);
        all_paths.insert("metrics", vec![all_paths::API_METRICS]);

        let server = TestServer::configure()
            .with_auth()
            .with_disable_authz(resources_disabled.clone())
            .spawn()
            .await;

        let client = server.http_client();
        let base = server.client_addr();

        // all disabled resources should be accessible
        for resource in &resources_disabled {
            let paths = all_paths
                .get(resource.as_str())
                .expect("resource to be mapped");
            for path in paths {
                assert_eq!(
                    client
                        .get(format!("{base}{path}"))
                        .send()
                        .await
                        .unwrap()
                        .status(),
                    StatusCode::OK
                );
            }
        }

        // and anything that is not disabled should not be accessible
        let resources_restricted = allowed_resources
            .iter()
            .filter(|path| !resources_disabled.contains(path))
            .map(|path| path.to_owned())
            .collect::<Vec<String>>();

        let token = server
            .auth_token
            .clone()
            .expect("admin token to have been present");

        for resource in &resources_restricted {
            let paths = all_paths
                .get(resource.as_str())
                .expect("resource to be mapped");

            for path in paths {
                // needs token to access
                assert_eq!(
                    client
                        .get(format!("{base}{path}"))
                        .send()
                        .await
                        .unwrap()
                        .status(),
                    StatusCode::UNAUTHORIZED
                );

                // with token it's fine to access
                assert_eq!(
                    client
                        .get(format!("{base}{path}"))
                        .bearer_auth(&token)
                        .send()
                        .await
                        .unwrap()
                        .status(),
                    StatusCode::OK
                );
            }
        }
    }

    // just useful to know what combos were used
    info!(
        ?all_possible_disable_auth_resources,
        "finished running the tests for"
    );
}
