//! Wire format (JSON over WebSocket text frames). Mirrored in web/src/protocol.ts.
//!
//! OpId:   [counter, replica]
//! Insert: {"id":[c,r],"s":seq,"i":{"l":OpId|null,"r":OpId|null,"c":"x"}}
//! Delete: {"id":[c,r],"s":seq,"d":OpId}
//! VV:     {"<replica>": seq, ...}
//!
//! Client -> server
//!   {"t":"hello","doc":name,"replica":r,"vv":VV}
//!   {"t":"ops","ops":[Op...]}
//!   {"t":"presence","name":s,"color":s,"cursor":OpId|null}
//! Server -> client
//!   {"t":"welcome","ops":[Op...],"vv":VV,"peers":[Presence...]}
//!   {"t":"ops","ops":[Op...]}
//!   {"t":"presence","replica":r,"name":s,"color":s,"cursor":OpId|null}
//!   {"t":"leave","replica":r}
//!   {"t":"error","msg":s}

use crate::json::Json;
use crdt_core::{Op, OpId, OpKind, VersionVector};

pub fn id_to_json(id: OpId) -> Json {
    Json::Arr(vec![
        Json::Num(id.counter as f64),
        Json::Num(id.replica as f64),
    ])
}

pub fn id_from_json(j: &Json) -> Result<OpId, String> {
    match j.as_arr() {
        Some([c, r]) => Ok(OpId {
            counter: c.as_u64().ok_or("bad counter")?,
            replica: r.as_u64().ok_or("bad replica")?,
        }),
        _ => Err("bad op id".into()),
    }
}

fn opt_id_to_json(id: Option<OpId>) -> Json {
    id.map_or(Json::Null, id_to_json)
}

fn opt_id_from_json(j: Option<&Json>) -> Result<Option<OpId>, String> {
    match j {
        None | Some(Json::Null) => Ok(None),
        Some(j) => id_from_json(j).map(Some),
    }
}

pub fn op_to_json(op: &Op) -> Json {
    let body = match &op.kind {
        OpKind::Insert {
            origin_left,
            origin_right,
            ch,
        } => (
            "i".to_string(),
            Json::Obj(vec![
                ("l".into(), opt_id_to_json(*origin_left)),
                ("r".into(), opt_id_to_json(*origin_right)),
                ("c".into(), Json::Str(ch.to_string())),
            ]),
        ),
        OpKind::Delete { target } => ("d".to_string(), id_to_json(*target)),
    };
    Json::Obj(vec![
        ("id".into(), id_to_json(op.id)),
        ("s".into(), Json::Num(op.seq as f64)),
        body,
    ])
}

pub fn op_from_json(j: &Json) -> Result<Op, String> {
    let id = id_from_json(j.get("id").ok_or("missing id")?)?;
    let seq = j.get("s").and_then(Json::as_u64).ok_or("missing seq")?;
    if seq == 0 || id.counter == 0 {
        return Err("seq and counter start at 1".into());
    }
    let kind = if let Some(ins) = j.get("i") {
        let s = ins.get("c").and_then(Json::as_str).ok_or("missing char")?;
        let mut chars = s.chars();
        let ch = match (chars.next(), chars.next()) {
            (Some(c), None) => c,
            _ => return Err("insert must carry exactly one character".into()),
        };
        OpKind::Insert {
            origin_left: opt_id_from_json(ins.get("l"))?,
            origin_right: opt_id_from_json(ins.get("r"))?,
            ch,
        }
    } else if let Some(d) = j.get("d") {
        OpKind::Delete {
            target: id_from_json(d)?,
        }
    } else {
        return Err("op is neither insert nor delete".into());
    };
    Ok(Op { id, seq, kind })
}

pub fn ops_to_json(ops: &[Op]) -> Json {
    Json::Arr(ops.iter().map(op_to_json).collect())
}

pub fn ops_from_json(j: &Json) -> Result<Vec<Op>, String> {
    j.as_arr()
        .ok_or("ops must be an array")?
        .iter()
        .map(op_from_json)
        .collect()
}

pub fn vv_to_json(vv: &VersionVector) -> Json {
    let mut kv: Vec<(String, Json)> = vv
        .iter()
        .map(|(r, s)| (r.to_string(), Json::Num(*s as f64)))
        .collect();
    kv.sort_by(|a, b| a.0.cmp(&b.0));
    Json::Obj(kv)
}

pub fn vv_from_json(j: Option<&Json>) -> Result<VersionVector, String> {
    let mut vv = VersionVector::new();
    match j {
        None | Some(Json::Null) => {}
        Some(Json::Obj(kv)) => {
            for (k, v) in kv {
                let r: u64 = k.parse().map_err(|_| "bad replica key")?;
                vv.insert(r, v.as_u64().ok_or("bad seq")?);
            }
        }
        _ => return Err("vv must be an object".into()),
    }
    Ok(vv)
}

pub fn msg(t: &str, fields: Vec<(&str, Json)>) -> Json {
    let mut kv = vec![("t".to_string(), Json::Str(t.into()))];
    kv.extend(fields.into_iter().map(|(k, v)| (k.to_string(), v)));
    Json::Obj(kv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crdt_core::Doc;

    #[test]
    fn op_roundtrip() {
        let mut d = Doc::new(4503599627370495); // 2^52 - 1
        d.insert_str(0, "a😀\"");
        d.delete(1);
        for op in d.log() {
            let s = op_to_json(op).to_string();
            assert_eq!(&op_from_json(&Json::parse(&s).unwrap()).unwrap(), op);
        }
        let vv = d.version().clone();
        assert_eq!(vv_from_json(Some(&vv_to_json(&vv))).unwrap(), vv);
    }

    #[test]
    fn rejects_multi_char_insert() {
        let j = Json::parse(r#"{"id":[1,2],"s":1,"i":{"l":null,"r":null,"c":"ab"}}"#).unwrap();
        assert!(op_from_json(&j).is_err());
    }
}
