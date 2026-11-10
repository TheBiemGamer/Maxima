use crate::lsx::{
    connection::LockedConnectionState,
    request::LSXRequestError,
    types::{LSXAchievementSets, LSXQueryAchievements, LSXResponseType},
};

/// Maxima doesn't track achievements, so every query gets an empty set list.
/// Answering at all matters: some games wait for the response.
pub async fn handle_query_achievements_request(
    _: LockedConnectionState,
    request: LSXQueryAchievements,
) -> Result<Option<LSXResponseType>, LSXRequestError> {
    Ok(Some(query_achievements_response(request)))
}

fn query_achievements_response(request: LSXQueryAchievements) -> LSXResponseType {
    LSXResponseType::AchievementSets(LSXAchievementSets {
        attr_PersonaId: request.attr_PersonaId,
        attr_GameId: request.attr_GameId,
        achievement_set: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsx::types::{LSXMessageType, LSXRequestType, LSXResponse, LSX};

    #[test]
    fn query_achievements_parses_and_gets_empty_response() {
        let xml = r#"<LSX><Request recipient="EbisuSDK" id="7"><QueryAchievements PersonaId="1001" GameId="Origin.OFR.50.0004321" All="true" Mode="ALL"/></Request></LSX>"#;
        let lsx: LSX = quick_xml::de::from_str(xml).unwrap();
        let LSXMessageType::Request(request) = lsx.value else {
            panic!("expected a request");
        };
        let LSXRequestType::QueryAchievements(query) = request.value else {
            panic!("expected QueryAchievements");
        };
        assert_eq!(query.attr_PersonaId.as_deref(), Some("1001"));

        let response = LSX {
            value: LSXMessageType::Response(LSXResponse {
                sender: request.recipient,
                id: request.id,
                value: query_achievements_response(query),
            }),
        };
        let out = quick_xml::se::to_string(&response).unwrap();
        assert!(out.contains("<AchievementSets"), "{out}");
        assert!(out.contains(r#"PersonaId="1001""#), "{out}");
        assert!(out.contains(r#"id="7""#), "{out}");
        assert!(!out.contains("<AchievementSet "), "{out}");
    }

    #[test]
    fn query_achievements_without_attributes_parses() {
        let xml =
            r#"<LSX><Request recipient="EbisuSDK" id="8"><QueryAchievements/></Request></LSX>"#;
        let lsx: LSX = quick_xml::de::from_str(xml).unwrap();
        let LSXMessageType::Request(request) = lsx.value else {
            panic!("expected a request");
        };
        let LSXRequestType::QueryAchievements(query) = request.value else {
            panic!("expected QueryAchievements");
        };
        let out = quick_xml::se::to_string(&query_achievements_response(query)).unwrap();
        assert!(!out.contains("PersonaId"), "{out}");
    }
}
