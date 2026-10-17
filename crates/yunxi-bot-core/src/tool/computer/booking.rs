//! Checkout reviews and receipts use the same private, live browser observation as computer_act.
use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Quote {
    kind: String,
    selection: String,
    dates: Vec<String>,
    party: Vec<String>,
    total: String,
    cancellation: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    observation_id: String,
    target_id: String,
    tab_id: String,
    quote: Quote,
    #[serde(rename = "ref")]
    reference: Option<String>,
    order_id: Option<String>,
    status_text: Option<String>,
}

pub(super) struct Review {
    pub action: Value,
    pub preview: String,
    pub identity: Value,
    pub valid_until: Instant,
}

pub(super) fn parameters(receipt: bool) -> Value {
    let mut properties = json!({
        "observation_id":{"type":"string"},"target_id":{"type":"string"},"tab_id":{"type":"string"},
        "quote":{"type":"object","additionalProperties":false,"properties":{
            "kind":{"type":"string","enum":["rail","hotel"]},
            "selection":{"type":"string","description":"当前网页原文：具体车次/席别或酒店/房型/餐食，不能用搜索摘要"},
            "dates":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":2,"description":"完整年月日；火车1项，酒店入住/退房2项。酒店结算页省略年份时仅可用适配器回传的真实日期来源，仍须当前酒店、月日及间数晚数一致；不得从任务或优惠券猜年份。"},
            "party":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":9,"description":"网页实际选择的旅客/入住人与人数间数摘录，不传证件、联系方式"},
            "total":{"type":"string","description":"网页实际全部应付金额原文，含币种、税费；不能填起价/每晚价"},
            "cancellation":{"type":"string","description":"网页实际退改/取消期限与费用原文；缺失先查规则，不猜"}
        },"required":["kind","selection","dates","party","total","cancellation"]}
    });
    let mut required = vec!["observation_id", "target_id", "tab_id", "quote"];
    if receipt {
        properties["quote"]["required"]
            .as_array_mut()
            .expect("literal array")
            .retain(|value| value != "cancellation");
        properties["quote"]["properties"]
            .as_object_mut()
            .expect("literal object")
            .remove("cancellation");
        properties["order_id"] = json!({"type":"string","description":"订单详情/付款页实际订单号"});
        properties["status_text"] = json!({"type":"string","description":"当前页面独立状态节点原文，如待支付/已出票/已取消；不是按钮文字"});
        required.extend(["order_id", "status_text"]);
    } else {
        properties["ref"] =
            json!({"type":"string","description":"最终提交控件的最新ref；不接受坐标或支付按钮"});
        required.push("ref");
    }
    json!({"type":"object","additionalProperties":false,"properties":properties,"required":required})
}

fn request(args: &Value) -> Result<Request> {
    if args.to_string().len() > 12_000 {
        return Err(bad("预订核对参数过长，不传完整网页或证件"));
    }
    serde_json::from_value(args.clone()).map_err(|_| bad("预订核对参数非法，不接受凭证或额外字段"))
}

fn action(i: &Request) -> Value {
    json!({"operation":"browser_click","observation_id":i.observation_id,
        "arguments":{"target_id":i.target_id,"tab_id":i.tab_id,"ref":i.reference},
        "purpose":"按已核对的实际报价提交本次订单，仍需本次人工批准；不授权支付"})
}

fn quote(i: &Request, frame: Option<&Frame>, epoch: &str, require_terms: bool) -> Result<Value> {
    check_frame(frame, &input(&action(i))?, epoch)?;
    let frame = frame.ok_or_else(|| bad("先观察实际订单页面"))?;
    let q = &i.quote;
    if !matches!(q.kind.as_str(), "rail" | "hotel")
        || q.dates.len() != if q.kind == "hotel" { 2 } else { 1 }
        || !(1..=9).contains(&q.party.len())
    {
        return Err(bad(
            "仅接入火车票和酒店；须提供完整日期及实际选择的人数/间数",
        ));
    }
    let dates = q
        .dates
        .iter()
        .map(|s| {
            ["%Y-%m-%d", "%Y年%m月%d日", "%Y/%m/%d"]
                .iter()
                .find_map(|format| chrono::NaiveDate::parse_from_str(s, format).ok())
                .ok_or_else(|| bad("日期摘录须含完整年月日，不能用今天/明天或省略年份"))
        })
        .collect::<Result<Vec<_>>>()?;
    if dates.len() == 2 && (dates[1] <= dates[0] || (dates[1] - dates[0]).num_days() > 30) {
        return Err(bad("酒店退房须晚于入住且最多30晚"));
    }
    let terms = if require_terms {
        Some(
            q.cancellation
                .as_ref()
                .ok_or_else(|| bad("提交前须核对真实退改/取消条件"))?,
        )
    } else {
        None
    };
    for (field, text) in [("selection", &q.selection), ("total", &q.total)]
        .into_iter()
        .chain(terms.map(|text| ("cancellation", text)))
        .chain(q.party.iter().map(|text| ("party", text)))
    {
        if text.trim().is_empty()
            || text.len() > 2000
            || text.chars().any(char::is_control)
            || !frame.text.contains(text.as_str())
        {
            return Err(bad(&format!(
                "预订核对字段{field}缺失或未匹配当前网页原文；重新观察并取实际摘录，不补造条件"
            )));
        }
    }
    if !q.dates.iter().all(|text| frame.text.contains(text))
        && !(q.kind == "hotel"
            && frame
                .hotel_dates
                .as_ref()
                .is_some_and(|source| source.matches(frame, &dates)))
    {
        return Err(bad(
            "预订核对字段dates未匹配完整原文或可追溯的同标签日期来源；重新读取真实详情并选择，不从任务或优惠券补年份",
        ));
    }
    if !q.total.chars().any(|c| c.is_ascii_digit())
        || !["¥", "￥", "元", "CNY", "RMB"]
            .iter()
            .any(|currency| q.total.contains(currency))
    {
        return Err(bad(
            "金额原文须有人民币币种及数字；总金额与税费仍由用户核对",
        ));
    }
    // Price and terms can change; neither makes an attempted identical itinerary safe to resubmit.
    Ok(json!({"kind":q.kind,"selection":q.selection,"dates":q.dates,"party":q.party}))
}

fn url(frame: &Frame) -> &str {
    frame.arguments["page"]["url"]
        .as_str()
        .or_else(|| frame.arguments["url"].as_str())
        .unwrap_or("")
}

fn platform(url: &str, kind: &str, checkout: bool) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let Some((host, path)) = rest.split_once('/') else {
        return false;
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    match (kind, host, checkout) {
        ("rail", "kyfw.12306.cn", true) => path == "otn/confirmPassenger/initDc",
        ("hotel", "hotels.ctrip.com", true) => path == "hotels/booknew",
        ("rail", "kyfw.12306.cn", false) => path.starts_with("otn/"),
        ("hotel", "hotels.ctrip.com" | "my.ctrip.com" | "order.ctrip.com", false) => {
            !path.is_empty()
        }
        _ => false,
    }
}

fn submit_name(kind: &str, name: &str) -> bool {
    matches!(name, "提交订单" | "确认订单" | "确认提交" | "提交" | "确认")
        || (kind == "hotel" && name == "最后一步")
}

pub(super) fn is_submission(frame: &Frame, input: &Input) -> bool {
    let kind = if platform(url(frame), "hotel", true) {
        "hotel"
    } else if platform(url(frame), "rail", true) {
        "rail"
    } else {
        return false;
    };
    input.operation == "browser_click"
        && frame.arguments["refs"].as_array().is_some_and(|nodes| {
            nodes.iter().any(|node| {
                node["ref"].is_string()
                    && node["ref"] == input.arguments["ref"]
                    && matches!(node["role"].as_str(), Some("button" | "link"))
                    && node["name"]
                        .as_str()
                        .is_some_and(|name| submit_name(kind, name))
            })
        })
}

pub(super) fn prepare(
    args: &Value,
    frame: Option<&Frame>,
    epoch: &str,
    attempts: &[Value],
) -> Result<Review> {
    let i = request(args)?;
    if i.order_id.is_some()
        || i.status_text.is_some()
        || i.reference.as_deref().is_none_or(str::is_empty)
    {
        return Err(bad("核对提交须给ref，不能混入订单回执参数"));
    }
    let mut identity = quote(&i, frame, epoch, true)?;
    let frame = frame.ok_or_else(|| bad("先观察实际结算页面"))?;
    if !platform(url(frame), &i.quote.kind, true) {
        return Err(bad("未绑定受支持平台的实际结算页，不执行跳转或提交"));
    }
    let node = frame.arguments["refs"]
        .as_array()
        .and_then(|nodes| {
            nodes
                .iter()
                .find(|node| node["ref"].as_str() == i.reference.as_deref())
        })
        .ok_or_else(|| bad("提交控件不在当前观察中"))?;
    if !matches!(node["role"].as_str(), Some("button" | "link"))
        || !node["name"]
            .as_str()
            .is_some_and(|name| submit_name(&i.quote.kind, name))
    {
        return Err(bad(
            "不是已接入的提交控件；支付、协议或未知按钮须停止并由用户接管",
        ));
    }
    let name = node["name"]
        .as_str()
        .ok_or_else(|| bad("提交控件缺少实际名称"))?;
    if name == "确认"
        && !attempts.iter().any(|attempt| {
            let mut previous = attempt.clone();
            if let Some(object) = previous.as_object_mut() {
                object.remove("submit_control");
            }
            previous == identity
        })
    {
        return Err(bad(
            "二次确认缺少本进程先前的提交记录，不把普通确认按钮当作下单",
        ));
    }
    identity["submit_control"] = json!(name);
    if attempts.contains(&identity) {
        return Err(bad(
            "delivery_unknown_or_order_exists：本进程已派发过同一预订阶段，先查实际订单；禁止重复提交",
        ));
    }
    let q = &i.quote;
    let date_source = if q.dates.iter().all(|date| frame.text.contains(date)) {
        String::new()
    } else {
        frame
            .hotel_dates
            .as_ref()
            .map(|source| source.source())
            .unwrap_or_default()
    };
    let mut valid_until = frame.at + Duration::from_secs(120);
    if !date_source.is_empty()
        && let Some(source) = frame.hotel_dates.as_ref()
    {
        valid_until = valid_until.min(source.valid_until());
    }
    Ok(Review {
        action: action(&i),
        identity,
        valid_until,
        preview: format!(
            "真实下单核对（网页摘录匹配，不是供应商API校验，仍须人工核对所选项/总金额/条款）\n平台：{}\n产品：{}\n日期：{}\n{}\n人员/数量：{}\n全部应付金额：{}\n退改/取消：{}\n实际提交控件：{}\n可能占用库存并产生取消费用。本次不授权支付。",
            url(frame),
            q.selection,
            q.dates.join(" / "),
            date_source,
            q.party.join(" / "),
            q.total,
            q.cancellation.as_deref().unwrap_or(""),
            name
        ),
    })
}

fn exact_status(frame: &Frame, status: &str) -> bool {
    frame.arguments["refs"].as_array().is_some_and(|nodes| {
        nodes.iter().any(|node| {
            matches!(
                node["role"].as_str(),
                Some("statictext" | "text" | "status")
            ) && node["name"] == status
        })
    }) || frame.text.lines().any(|line| {
        let line = line.trim().trim_start_matches("- ");
        ["statictext ", "text ", "status "].iter().any(|prefix| {
            line.strip_prefix(prefix)
                .is_some_and(|s| s == format!("{status:?}"))
        })
    })
}

pub(super) fn read_order(args: &Value, frame: Option<&Frame>, epoch: &str) -> Result<Value> {
    let i = request(args)?;
    quote(&i, frame, epoch, false)?;
    let frame = frame.ok_or_else(|| bad("先观察实际订单页"))?;
    let id = i
        .order_id
        .as_deref()
        .ok_or_else(|| bad("订单号缺失，不把点击成功当作下单成功"))?;
    let status = i
        .status_text
        .as_deref()
        .ok_or_else(|| bad("订单状态缺失，继续查订单"))?;
    if i.reference.is_some()
        || !platform(url(frame), &i.quote.kind, false)
        || !(6..=64).contains(&id.len())
        || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        || !frame
            .text
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .any(|token| token == id)
        || !exact_status(frame, status)
    {
        return Err(bad(
            "订单号、状态节点或平台无法从本次页面核对；结果仍不明，禁止重新下单",
        ));
    }
    let (state, paid) = match status {
        "待支付" | "未支付" | "待付款" => ("pending_payment", false),
        "已出票" | "预订成功" | "已确认" => ("confirmed", false),
        "已支付" | "支付成功" | "已付款" => ("paid", true),
        "已取消" | "交易关闭" | "订单取消成功" => ("cancelled", false),
        _ => {
            return Err(bad(
                "未知订单状态，保留原文待人工核对，不猜测已支付或已预订",
            ));
        }
    };
    Ok(
        json!({"order_id":id,"state":state,"status_text":status,"paid":paid,
        "payment_status":if paid {"paid"} else if state=="pending_payment" {"unpaid"} else {"unknown"},
        "source_url":url(frame),"quote":{"kind":i.quote.kind,"selection":i.quote.selection,"dates":i.quote.dates,"party":i.quote.party,"total":i.quote.total},"observation_id":i.observation_id,
        "schedule_ready":false,"next_step":if state=="pending_payment" {"已查到待支付订单；付款由用户接管，不能标为已出票/已确认住宿。"} else {"仅回读本次网页状态；日程仍须核对真实行程，mac_trip_plan/apply审批写入，不自动付款、取消或退款。"}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Frame, Value) {
        let quote = json!({"kind":"rail","selection":"G7 北京南→上海虹桥 二等座","dates":["2026-10-20"],"party":["合成乘客甲 成人票"],"total":"¥661.00","cancellation":"开车前8天以上退票不收手续费"});
        let text = "G7 北京南→上海虹桥 二等座\n2026-10-20\n合成乘客甲 成人票\n¥661.00\n开车前8天以上退票不收手续费";
        let frame = Frame {
            hotel_dates: None,
            id: "v1".into(),
            at: Instant::now(),
            text: text.into(),
            arguments: json!({"target_id":"t","tab_id":"tab","page":{"url":"https://kyfw.12306.cn/otn/confirmPassenger/initDc"},"refs":[{"ref":"submit","role":"button","name":"提交订单"}]}),
        };
        (
            frame,
            json!({"observation_id":"v1","target_id":"t","tab_id":"tab","ref":"submit","quote":quote}),
        )
    }

    #[test]
    fn checkout_requires_live_matching_quote_and_never_grants_approval() {
        let (frame, args) = fixture();
        let review = prepare(&args, Some(&frame), "v1", &[]).unwrap();
        assert!(review.preview.contains("¥661.00"));
        assert!(review.preview.contains("开车前8天"));
        assert_eq!(review.action["operation"], "browser_click");
        assert_eq!(review.action["observation_id"], "v1");
        for (key, value) in [
            ("total", json!("¥600.00")),
            ("party", json!(["合成乘客乙"])),
            ("cancellation", json!("免费退改")),
            ("dates", json!([])),
            ("kind", json!("flight")),
        ] {
            let mut wrong = args.clone();
            wrong["quote"][key] = value;
            assert!(prepare(&wrong, Some(&frame), "v1", &[]).is_err(), "{key}");
        }
        assert!(prepare(&args, Some(&frame), "other-process", &[]).is_err());
        assert!(prepare(&args, None, "v1", &[]).is_err());
        let mut wrong = args.clone();
        wrong["tab_id"] = json!("other");
        assert!(prepare(&wrong, Some(&frame), "v1", &[]).is_err());
        let mut old = frame;
        old.at = Instant::now() - Duration::from_secs(121);
        assert!(prepare(&args, Some(&old), "v1", &[]).is_err());
    }

    #[test]
    fn a_quote_mismatch_identifies_the_field_without_echoing_private_content() {
        let (frame, args) = fixture();
        for (field, value, private) in [
            ("selection", json!("private-product"), "private-product"),
            ("total", json!("¥123.45"), "¥123.45"),
            ("cancellation", json!("private-terms"), "private-terms"),
            ("dates", json!(["2026-10-22"]), "2026-10-22"),
            ("party", json!(["private-person"]), "private-person"),
        ] {
            let mut wrong = args.clone();
            wrong["quote"][field] = value;
            let error = prepare(&wrong, Some(&frame), "v1", &[])
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains(field));
            assert!(!error.contains(private));
        }
    }

    #[test]
    fn wrong_platform_payment_controls_and_repeat_submission_fail_closed() {
        let (mut frame, args) = fixture();
        let review = prepare(&args, Some(&frame), "v1", &[]).unwrap();
        assert!(
            prepare(
                &args,
                Some(&frame),
                "v1",
                std::slice::from_ref(&review.identity)
            )
            .is_err()
        );
        let mut repriced = args.clone();
        repriced["quote"]["total"] = json!("¥662.00");
        frame.text.push_str("\n¥662.00");
        assert!(prepare(&repriced, Some(&frame), "v1", &[review.identity]).is_err());
        for url in [
            "https://kyfw.12306.cn.attacker.example/otn/confirmPassenger/initDc",
            "https://user@kyfw.12306.cn/otn/confirmPassenger/initDc",
            "https://kyfw.12306.cn/otn/leftTicket/init",
        ] {
            frame.arguments["page"]["url"] = json!(url);
            assert!(prepare(&args, Some(&frame), "v1", &[]).is_err());
        }
        frame.arguments["page"]["url"] = json!("https://kyfw.12306.cn/otn/confirmPassenger/initDc");
        for name in ["立即支付", "确认支付", "同意并购买", "无名称"] {
            frame.arguments["refs"][0]["name"] = json!(name);
            assert!(prepare(&args, Some(&frame), "v1", &[]).is_err());
        }
    }

    #[test]
    fn receipts_need_order_number_and_actual_status_not_just_a_successful_click() {
        let (mut frame, args) = fixture();
        frame.arguments["page"]["url"] = json!("https://kyfw.12306.cn/otn/payOrder/init");
        frame
            .text
            .push_str("\n- statictext \"E123456789\"\n- statictext \"待支付\"");
        let receipt = json!({"observation_id":"v1","target_id":"t","tab_id":"tab","quote":args["quote"],"order_id":"E123456789","status_text":"待支付"});
        let out = read_order(&receipt, Some(&frame), "v1").unwrap();
        assert_eq!(out["state"], "pending_payment");
        assert_eq!(out["paid"], false);
        assert_eq!(out["schedule_ready"], false);
        let mut wrong = receipt.clone();
        wrong["order_id"] = json!("E987654321");
        assert!(read_order(&wrong, Some(&frame), "v1").is_err());
        wrong = receipt.clone();
        wrong["status_text"] = json!("已支付");
        assert!(read_order(&wrong, Some(&frame), "v1").is_err());
        let longer = frame.text.replace("E123456789", "E1234567890");
        let original = std::mem::replace(&mut frame.text, longer);
        assert!(
            read_order(&receipt, Some(&frame), "v1").is_err(),
            "partial order numbers cannot match"
        );
        frame.text = original;
        frame.text = frame.text.replace("\"待支付\"", "\"尚未支付成功\"");
        wrong["status_text"] = json!("支付成功");
        assert!(read_order(&wrong, Some(&frame), "v1").is_err());
        assert!(read_order(&receipt, Some(&frame), "stale").is_err());
    }

    #[test]
    fn payment_receipt_does_not_require_refund_terms_to_be_repeated_on_the_payment_page() {
        let (mut frame, args) = fixture();
        frame.arguments["page"]["url"] = json!("https://kyfw.12306.cn/otn/payOrder/init");
        frame.text = frame.text.replace("开车前8天以上退票不收手续费", "");
        frame
            .text
            .push_str("\n- statictext \"E123456789\"\n- statictext \"待支付\"");
        let mut receipt = json!({"observation_id":"v1","target_id":"t","tab_id":"tab","quote":args["quote"],"order_id":"E123456789","status_text":"待支付"});
        receipt["quote"]
            .as_object_mut()
            .unwrap()
            .remove("cancellation");
        assert_eq!(
            read_order(&receipt, Some(&frame), "v1").unwrap()["state"],
            "pending_payment"
        );
        let mut checkout = args;
        checkout["quote"]
            .as_object_mut()
            .unwrap()
            .remove("cancellation");
        assert!(prepare(&checkout, Some(&frame), "v1", &[]).is_err());
    }

    #[test]
    fn hotel_dates_must_include_a_real_year_and_checkout_after_checkin() {
        let (mut frame, mut args) = fixture();
        frame.arguments["page"]["url"] = json!("https://hotels.ctrip.com/hotels/booknew");
        args["quote"]["kind"] = json!("hotel");
        args["quote"]["dates"] = json!(["2026-10-20", "2026年10月21日"]);
        frame.text.push_str("\n2026年10月21日");
        assert!(prepare(&args, Some(&frame), "v1", &[]).is_ok());
        for dates in [
            json!(["2026-10-20", "2026-10-20"]),
            json!(["2026年10月21日", "2026-10-20"]),
            json!(["今天", "明天"]),
        ] {
            frame.text.push_str("\n今天\n明天");
            args["quote"]["dates"] = dates;
            assert!(prepare(&args, Some(&frame), "v1", &[]).is_err());
        }
    }

    #[test]
    fn a_proven_same_tab_selection_supplies_the_year_but_a_task_or_coupon_does_not() {
        let (source, action) = super::super::hotel_dates::tests::selection();
        let mut frame = super::super::hotel_dates::tests::checkout();
        let args = json!({"observation_id":"checkout","target_id":"t","tab_id":"tab","ref":"submit","quote":{"kind":"hotel","selection":"合成测试酒店","dates":["2026-10-20","2026-10-21"],"party":["合成住客甲","1间 x 1晚"],"total":"¥185.62","cancellation":"入住当天18:00前可免费取消"}});
        assert!(prepare(&args, Some(&frame), "checkout", &[]).is_err());
        let mut proof = super::super::hotel_dates::HotelDates::capture(&source, &action).unwrap();
        assert!(proof.advance(&frame));
        frame.hotel_dates = Some(proof);
        let review = prepare(&args, Some(&frame), "checkout", &[]).unwrap();
        assert!(review.preview.contains("酒店日期来源"));
        assert_eq!(review.valid_until, source.at + Duration::from_secs(120));
        assert!(review.preview.contains("本次不授权支付"));
        assert!(prepare(&args, Some(&frame), "checkout", &[review.identity]).is_err());
        frame.hotel_dates = None;
        assert!(prepare(&args, Some(&frame), "checkout", &[]).is_err());
    }

    #[test]
    fn ctrip_last_step_is_an_unapproved_hotel_submission_not_a_rail_confirmation() {
        let (mut frame, mut args) = fixture();
        frame.arguments["refs"][0]["name"] = json!("最后一步");
        assert!(prepare(&args, Some(&frame), "v1", &[]).is_err());
        frame.arguments["page"]["url"] = json!("https://hotels.ctrip.com/hotels/booknew");
        args["quote"]["kind"] = json!("hotel");
        args["quote"]["dates"] = json!(["2026-10-20", "2026-10-21"]);
        frame.text.push_str("\n2026-10-21");
        let review = prepare(&args, Some(&frame), "v1", &[]).unwrap();
        assert_eq!(review.action["operation"], "browser_click");
        assert!(review.preview.contains("仍须人工核对"));
        assert!(review.preview.contains("本次不授权支付"));
        assert!(prepare(&args, Some(&frame), "v1", &[review.identity]).is_err());
        frame.arguments["refs"][0]["name"] = json!("最后一步并支付");
        assert!(prepare(&args, Some(&frame), "v1", &[]).is_err());
    }

    #[test]
    fn rail_second_confirmation_is_a_separate_exact_human_action_not_a_retry() {
        let (mut frame, args) = fixture();
        let first = prepare(&args, Some(&frame), "v1", &[]).unwrap();
        frame.arguments["refs"][0]["name"] = json!("确认");
        assert!(prepare(&args, Some(&frame), "v1", &[]).is_err());
        let second = prepare(
            &args,
            Some(&frame),
            "v1",
            std::slice::from_ref(&first.identity),
        )
        .unwrap();
        assert_ne!(first.identity, second.identity);
        assert!(
            prepare(
                &args,
                Some(&frame),
                "v1",
                &[first.identity, second.identity]
            )
            .is_err()
        );
    }
}
