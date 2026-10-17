//! A year omitted at checkout can only come from the real selection that opened it.
use super::{Frame, Input};
use chrono::{Datelike, NaiveDate};
use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(super) struct HotelDates {
    target: String,
    tab: String,
    hotel_id: String,
    hotel_name: String,
    dates: [NaiveDate; 2],
    rooms: u8,
    at: Instant,
    pub epoch: String,
}

pub(super) fn page_url(frame: &Frame) -> &str {
    frame.arguments["page"]["url"]
        .as_str()
        .or_else(|| frame.arguments["url"].as_str())
        .unwrap_or("")
}

fn parameter<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    let mut values = query.split('&').filter_map(|part| {
        let (key, value) = part.split_once('=')?;
        key.eq_ignore_ascii_case(name).then_some(value)
    });
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

fn names(frame: &Frame) -> impl Iterator<Item = &str> {
    ["refs", "content_refs"].into_iter().flat_map(|key| {
        frame.arguments[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|node| node["name"].as_str())
    })
}

fn hotel_name(name: &str) -> String {
    name.trim().replace('（', "(").replace('）', ")")
}

fn token(text: &str, value: &str) -> bool {
    text.match_indices(value).any(|(at, _)| {
        at == 0
            || !text[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_digit())
    })
}

fn month_day(text: &str, date: NaiveDate) -> bool {
    [
        format!("{}月{}日", date.month(), date.day()),
        format!("{:02}月{:02}日", date.month(), date.day()),
        format!("{}月{:02}日", date.month(), date.day()),
        format!("{:02}月{}日", date.month(), date.day()),
    ]
    .iter()
    .any(|value| token(text, value))
}

impl HotelDates {
    pub fn valid_until(&self) -> Instant {
        self.at + Duration::from_secs(120)
    }

    pub fn acknowledged(&self, action: &Input, receipt: &Value) -> bool {
        receipt["status"] == "ok"
            && receipt["target_id"] == self.target
            && receipt["tab_id"] == self.tab
            && (action.operation != "browser_click" || receipt["ref"] == action.arguments["ref"])
    }

    pub fn capture(frame: &Frame, action: &Input) -> Option<Self> {
        if action.operation != "browser_click"
            || action.observation_id.as_deref() != Some(frame.id.as_str())
            || action.arguments.get("x").is_some()
            || action.arguments.get("y").is_some()
            || frame.at.elapsed() > Duration::from_secs(120)
        {
            return None;
        }
        let reference = action.arguments["ref"].as_str()?;
        let node = frame.arguments["refs"]
            .as_array()?
            .iter()
            .find(|node| node["ref"] == reference)?;
        if node["name"] != "预订"
            || !matches!(node["role"].as_str(), Some("button" | "link"))
            || node["states"]["disabled"] == true
        {
            return None;
        }
        let target = frame.arguments["target_id"].as_str()?;
        let tab = frame.arguments["tab_id"].as_str()?;
        if action.arguments["target_id"] != target || action.arguments["tab_id"] != tab {
            return None;
        }
        let query = page_url(frame).strip_prefix("https://hotels.ctrip.com/hotels/detail/?")?;
        let hotel_id = parameter(query, "hotelId")?;
        if hotel_id.is_empty()
            || hotel_id.len() > 20
            || !hotel_id.bytes().all(|c| c.is_ascii_digit())
            || hotel_id.bytes().all(|c| c == b'0')
        {
            return None;
        }
        // Only the actual platform's unambiguous date parameters are evidence, never model arguments.
        let parse_date = |key| {
            let value = parameter(query, key)?;
            (value.len() == 10).then(|| NaiveDate::parse_from_str(value, "%Y-%m-%d").ok())?
        };
        let dates = [parse_date("checkIn")?, parse_date("checkOut")?];
        if !(1..=30).contains(&(dates[1] - dates[0]).num_days()) {
            return None;
        }
        let rooms = parameter(query, "crn")?.parse::<u8>().ok()?;
        if !(1..=9).contains(&rooms) {
            return None;
        }
        let title = frame.arguments["page"]["title"].as_str()?;
        let name = hotel_name(title.split_once("预订价格")?.0);
        if !(2..=200).contains(&name.chars().count()) || name.chars().any(char::is_control) {
            return None;
        }
        Some(Self {
            target: target.into(),
            tab: tab.into(),
            hotel_id: hotel_id.into(),
            hotel_name: name,
            dates,
            rooms,
            at: frame.at,
            epoch: frame.id.clone(),
        })
    }

    fn accepts_checkout(&self, frame: &Frame) -> bool {
        // shortcut: Driver 0.36 has no opener proof; cross-tab checkout must stay refused.
        if self.at.elapsed() > Duration::from_secs(120)
            || frame.arguments["target_id"] != self.target
            || frame.arguments["tab_id"] != self.tab
            || page_url(frame).split(['?', '#']).next()
                != Some("https://hotels.ctrip.com/hotels/booknew")
        {
            return false;
        }
        if let Some((_, query)) = page_url(frame).split_once('?') {
            for (key, expected) in [
                ("hotelId", self.hotel_id.clone()),
                ("checkIn", self.dates[0].to_string()),
                ("checkOut", self.dates[1].to_string()),
                ("crn", self.rooms.to_string()),
            ] {
                if query.split('&').any(|part| {
                    part.split_once('=')
                        .is_some_and(|(name, _)| name.eq_ignore_ascii_case(key))
                }) && parameter(query, key) != Some(expected.as_str())
                {
                    return false;
                }
            }
        }
        true
    }

    pub fn advance(&mut self, frame: &Frame) -> bool {
        if !self.accepts_checkout(frame) {
            return false;
        }
        self.epoch = frame.id.clone();
        true
    }

    pub fn matches(&self, frame: &Frame, dates: &[NaiveDate]) -> bool {
        if self.epoch != frame.id || dates != self.dates || !self.accepts_checkout(frame) {
            return false;
        }
        let nights = (self.dates[1] - self.dates[0]).num_days();
        names(frame).any(|name| hotel_name(name) == self.hotel_name)
            && names(frame)
                .any(|name| month_day(name, self.dates[0]) && month_day(name, self.dates[1]))
            && names(frame).any(|name| {
                token(name, &format!("{}间", self.rooms)) && token(name, &format!("{nights}晚"))
            })
    }

    pub fn source(&self) -> String {
        format!(
            "酒店日期来源：https://hotels.ctrip.com/hotels/detail/?hotelId={}&checkIn={}&checkOut={}&crn={}；酒店：{}。来源由本进程实际预订按钮及同一标签衔接，结算页仍须核对同一酒店、月日与间数晚数；此来源不是提交批准。",
            self.hotel_id, self.dates[0], self.dates[1], self.rooms, self.hotel_name
        )
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use serde_json::json;

    pub fn selection() -> (Frame, Input) {
        let frame = Frame {
            id: "source".into(),
            at: Instant::now(),
            text: String::new(),
            hotel_dates: None,
            arguments: json!({"target_id":"t","tab_id":"tab","page":{"title":"合成测试酒店预订价格,联系电话位置地址【携程酒店】","url":"https://hotels.ctrip.com/hotels/detail/?hotelId=12345&checkIn=2026-10-20&checkOut=2026-10-21&crn=1"},"refs":[{"ref":"book","role":"button","name":"预订"}]}),
        };
        let action = super::super::input(&json!({"operation":"browser_click","observation_id":"source","arguments":{"target_id":"t","tab_id":"tab","ref":"book"},"purpose":"进入填写页，不提交订单"})).unwrap();
        (frame, action)
    }

    pub fn checkout() -> Frame {
        Frame {
            id: "checkout".into(),
            at: Instant::now(),
            text: "合成测试酒店\n2026年优惠券\n10月20日(周二) - 10月21日(周三)\n1间 x 1晚\n合成住客甲\n¥185.62\n入住当天18:00前可免费取消".into(),
            hotel_dates: None,
            arguments: json!({"target_id":"t","tab_id":"tab","page":{"url":"https://hotels.ctrip.com/hotels/booknew"},"refs":[{"ref":"submit","role":"button","name":"最后一步"}],"content_refs":[{"name":"合成测试酒店","role":"heading"},{"name":"10月20日(周二) - 10月21日(周三)","role":"statictext"},{"name":"1间 x 1晚","role":"statictext"}]}),
        }
    }

    #[test]
    fn only_a_real_booking_ref_with_unambiguous_platform_dates_can_capture_a_year() {
        let (frame, action) = selection();
        assert!(HotelDates::capture(&frame, &action).is_some());
        let proof = HotelDates::capture(&frame, &action).unwrap();
        let ack = json!({"status":"ok","target_id":"t","tab_id":"tab","ref":"book"});
        assert!(proof.acknowledged(&action, &ack));
        for (key, value) in [
            ("status", "unknown"),
            ("target_id", "other"),
            ("tab_id", "other"),
            ("ref", "other"),
        ] {
            let mut wrong = ack.clone();
            wrong[key] = json!(value);
            assert!(!proof.acknowledged(&action, &wrong));
        }
        assert!(!proof.acknowledged(&action, &json!({})));
        for url in [
            "https://hotels.ctrip.com.attacker.example/hotels/detail/?hotelId=12345&checkIn=2026-10-20&checkOut=2026-10-21&crn=1",
            "https://hotels.ctrip.com/hotels/detail/?hotelId=12345&checkIn=2026-10-20&checkIn=2027-10-20&checkOut=2026-10-21&crn=1",
            "https://hotels.ctrip.com/hotels/detail/?hotelId=12345&checkIn=2026-02-30&checkOut=2026-03-01&crn=1",
            "https://hotels.ctrip.com/hotels/detail/?hotelId=12345&checkIn=2026-10-21&checkOut=2026-10-20&crn=1",
        ] {
            let mut changed = selection().0;
            changed.arguments["page"]["url"] = json!(url);
            assert!(HotelDates::capture(&changed, &action).is_none());
        }
        for key in ["x", "y"] {
            let mut changed = selection().1;
            changed.arguments[key] = json!(20);
            assert!(HotelDates::capture(&frame, &changed).is_none());
        }
        let mut changed = selection().0;
        changed.arguments["refs"][0]["name"] = json!("提交订单");
        assert!(HotelDates::capture(&changed, &action).is_none());
    }

    #[test]
    fn another_tab_hotel_date_quantity_year_or_expired_evidence_never_matches() {
        let (source, action) = selection();
        let mut proof = HotelDates::capture(&source, &action).unwrap();
        let frame = checkout();
        assert!(proof.advance(&frame));
        assert!(proof.matches(&frame, &proof.dates));
        for (key, value) in [("target_id", "other"), ("tab_id", "other")] {
            let mut changed = checkout();
            changed.arguments[key] = json!(value);
            assert!(!proof.advance(&changed));
            assert!(!proof.matches(&changed, &proof.dates));
        }
        for url in [
            "https://hotels.ctrip.com/hotels/booknew?hotelId=99999",
            "https://hotels.ctrip.com/hotels/booknew?checkIn=2027-10-20",
            "https://hotels.ctrip.com/hotels/booknew?checkIn=2026-10-20&checkIn=2027-10-20",
            "https://hotels.ctrip.com/hotels/booknew?crn=2",
            "https://hotels.ctrip.com.attacker.example/hotels/booknew",
        ] {
            let mut changed = checkout();
            changed.arguments["page"]["url"] = json!(url);
            assert!(!proof.advance(&changed));
            assert!(!proof.matches(&changed, &proof.dates));
        }
        for (index, text) in [
            (0, "另一家酒店"),
            (1, "10月21日 - 10月22日"),
            (2, "11间 x 11晚"),
        ] {
            let mut changed = checkout();
            changed.arguments["content_refs"][index]["name"] = json!(text);
            assert!(!proof.matches(&changed, &proof.dates));
        }
        let wrong_year = [
            NaiveDate::from_ymd_opt(2027, 10, 20).unwrap(),
            NaiveDate::from_ymd_opt(2027, 10, 21).unwrap(),
        ];
        assert!(!proof.matches(&frame, &wrong_year));
        proof.at = Instant::now() - Duration::from_secs(121);
        assert!(!proof.advance(&frame));
        assert!(!proof.matches(&frame, &proof.dates));
    }
}
