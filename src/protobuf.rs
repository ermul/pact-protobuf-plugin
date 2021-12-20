//! Module for processing and comparing protobuf messages

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::anyhow;
use itertools::{Either, Itertools};
use log::{debug, LevelFilter, max_level, trace};
use maplit::hashmap;
use pact_models::generators::Generator;
use pact_models::matchingrules::MatchingRuleCategory;
use pact_models::matchingrules::expressions::parse_matcher_def;
use pact_models::path_exp::DocPath;
use pact_models::prelude::RuleLogic;
use pact_plugin_driver::proto::{
  Body,
  InteractionResponse,
  PluginConfiguration,
  MatchingRules,
  MatchingRule
};
use pact_plugin_driver::proto::body::ContentTypeHint;
use pact_plugin_driver::proto::interaction_response::MarkupType;
use pact_plugin_driver::utils::{proto_value_to_json, proto_value_to_string, to_proto_struct};
use prost_types::{DescriptorProto, field_descriptor_proto, FieldDescriptorProto, FileDescriptorProto, ServiceDescriptorProto};
use prost_types::field_descriptor_proto::Type;
use prost_types::value::Kind;
use serde_json::{json, Value};
use tokio::fs::File;
use tokio::io::AsyncReadExt;

use crate::message_builder::{MessageBuilder, MessageFieldValue, RType};
use crate::protoc::Protoc;
use crate::utils::{find_nested_type, is_repeated, last_name, proto_struct_to_btreemap, proto_type_name};

/// Process the provided protobuf file and configure the interaction
pub(crate) async fn process_proto(proto_file: String, protoc: &Protoc, config: BTreeMap<String, prost_types::Value>) -> anyhow::Result<(Vec<InteractionResponse>, PluginConfiguration)> {
  debug!("Parsing proto file '{}'", proto_file);
  let proto_file = Path::new(proto_file.as_str());
  let (descriptors, digest, descriptor_bytes) = protoc.parse_proto_file(proto_file).await?;
  debug!("Parsed proto file OK, file descriptors = {:?}", descriptors.file.iter().map(|file| file.name.as_ref()).collect_vec());

  let file_descriptors: HashMap<String, &FileDescriptorProto> = descriptors.file
    .iter().map(|des| (des.name.clone().unwrap_or_default(), des))
    .collect();
  let file_name = &*proto_file.file_name().unwrap_or_default().to_string_lossy();
  let descriptor = match file_descriptors.get(file_name) {
    None => return Err(anyhow!("Did not find a file proto descriptor for the provided proto file '{}'", file_name)),
    Some(des) => *des
  };

  if max_level() >= LevelFilter::Trace {
    trace!("All message types in proto descriptor");
    for message_type in &descriptor.message_type {
      trace!("  {:?}", message_type.name);
    }
  }

  let descriptor_encoded = base64::encode(&descriptor_bytes);
  let descriptor_hash = format!("{:x}", md5::compute(&descriptor_bytes));
  let mut interactions = vec![];

  if let Some(message_type) = config.get("pact:message-type") {
    let message = proto_value_to_string(message_type)
      .ok_or_else(|| anyhow!("Did not get a valid value for 'pact:message-type'. It should be a string"))?;
    let result = configure_protobuf_message(message.as_str(), config, descriptor, &file_descriptors, proto_file, descriptor_hash.as_str())?;
    interactions.push(result);
  } else if let Some(service_name) = config.get("pact:proto-service") {
    let service_name = proto_value_to_string(service_name)
      .ok_or_else(|| anyhow!("Did not get a valid value for 'pact:proto-service'. It should be a string"))?;
    let (request_part, response_part) = configure_protobuf_service(service_name, config, descriptor, &file_descriptors, proto_file, descriptor_hash.as_str())?;
    interactions.push(request_part);
    interactions.push(response_part);
  }

  let mut f = File::open(proto_file).await?;
  let mut file_contents = String::new();
  f.read_to_string(&mut file_contents).await?;

  let digest_str = format!("{:x}", digest);
  let plugin_config = PluginConfiguration {
    interaction_configuration: None,
    pact_configuration: Some(to_proto_struct(hashmap!{
      digest_str => json!({
        "protoFile": file_contents,
        "protoDescriptors": descriptor_encoded
      })
    }))
  };

  Ok((interactions, plugin_config))
}

/// Configure the interaction for a Protobuf service method, which has an input and output message
fn configure_protobuf_service(
  service_name: String,
  config: BTreeMap<String, prost_types::Value>,
  descriptor: &FileDescriptorProto,
  all_descriptors: &HashMap<String, &FileDescriptorProto>,
  proto_file: &Path,
  descriptor_hash: &str
) -> anyhow::Result<(InteractionResponse, InteractionResponse)> {
  debug!("Looking for service and method with name '{}'", service_name);
  let service_and_proc = service_name.split_once('/')
    .ok_or_else(|| anyhow!("Service name '{}' is not valid, it should be of the form <SERVICE>/<METHOD>", service_name))?;
  let service_descriptor = descriptor.service
    .iter().find(|p| p.name.clone().unwrap_or_default() == service_and_proc.0)
    .ok_or_else(|| anyhow!("Did not find a descriptor for service '{}'", service_name))?;
  construct_protobuf_interaction_for_service(service_descriptor, config, service_and_proc.0, service_and_proc.1, all_descriptors)
    .map(|(request, response)| {
      let plugin_configuration = Some(PluginConfiguration {
        interaction_configuration: Some(to_proto_struct(hashmap! {
            "service".to_string() => Value::String(service_name.to_string()),
            "descriptorKey".to_string() => Value::String(descriptor_hash.to_string())
          })),
        pact_configuration: None
      });
      (
        InteractionResponse { plugin_configuration: plugin_configuration.clone(), .. request },
        InteractionResponse { plugin_configuration, .. response }
      )
    })
}

/// Constructs an interaction for the given Protobuf service descriptor
fn construct_protobuf_interaction_for_service(
  descriptor: &ServiceDescriptorProto,
  config: BTreeMap<String, prost_types::Value>,
  service_name: &str,
  method_name: &str,
  all_descriptors: &HashMap<String, &FileDescriptorProto>
) -> anyhow::Result<(InteractionResponse, InteractionResponse)> {
  if !config.contains_key("response") {
    return Err(anyhow!("A Protobuf service requires a 'response' configuration"))
  }

  let method_descriptor = descriptor.method.iter()
    .find(|m| m.name.clone().unwrap_or_default() == method_name)
    .ok_or_else(|| anyhow!("Did not find a method descriptor for method '{}' in service '{}'", method_name, service_name))?;

  let input_name = method_descriptor.input_type.as_ref().ok_or_else(|| anyhow!("Input message name is empty for service {}/{}", service_name, method_name))?;
  let output_name = method_descriptor.output_type.as_ref().ok_or_else(|| anyhow!("Input message name is empty for service {}/{}", service_name, method_name))?;
  let input_message_name = last_name(input_name.as_str());
  let request_descriptor = find_message_descriptor(input_message_name, all_descriptors)?;
  let output_message_name = last_name(output_name.as_str());
  let response_descriptor = find_message_descriptor(output_message_name, all_descriptors)?;

  let request_part = config.get("request").map(|request_config| {
    request_config.kind.as_ref().map(|kind| {
      match kind {
        Kind::StructValue(s) => Some(proto_struct_to_btreemap(s)),
        _ => None
      }
    }).flatten()
  })
    .flatten()
    .map(|config| construct_protobuf_interaction_for_message(&request_descriptor, config, input_message_name, "request"))
    .ok_or_else(|| anyhow!("A Protobuf service requires a 'request' configuration in map format"))??;

  let response_part = config.get("response").map(|response_config| {
    response_config.kind.as_ref().map(|kind| {
      match kind {
        Kind::StructValue(s) => Some(proto_struct_to_btreemap(s)),
        _ => None
      }
    }).flatten()
  })
    .flatten()
    .map(|config| construct_protobuf_interaction_for_message(&response_descriptor, config, output_message_name, "response"))
    .ok_or_else(|| anyhow!("A Protobuf service requires a 'response' configuration in map format"))??;

  Ok((request_part, response_part))
}

fn find_message_descriptor(message_name: &str, all_descriptors: &HashMap<String, &FileDescriptorProto>) -> anyhow::Result<DescriptorProto> {
  all_descriptors.values().map(|descriptor| {
    descriptor.message_type.iter()
      .find(|p| p.name.clone().unwrap_or_default() == message_name)
  }).find(|d| d.is_some())
    .flatten()
    .cloned()
    .ok_or_else(|| anyhow!("Did not find the descriptor for message {}", message_name))
}

/// Configure the interaction for a single Protobuf message
fn configure_protobuf_message(
  message_name: &str,
  config: BTreeMap<String, prost_types::Value>,
  descriptor: &FileDescriptorProto,
  all_descriptors: &HashMap<String, &FileDescriptorProto>,
  proto_file: &Path,
  descriptor_hash: &str
) -> anyhow::Result<InteractionResponse> {
  trace!(">> configure_protobuf_message({}, _, _, _, {:?}, {})", message_name, proto_file, descriptor_hash);
  debug!("Looking for message of type '{}'", message_name);
  let message_descriptor = descriptor.message_type
    .iter().find(|p| p.name.clone().unwrap_or_default() == message_name)
    .ok_or_else(|| anyhow!("Did not find a descriptor for message '{}'", message_name))?;
  construct_protobuf_interaction_for_message(message_descriptor, config, message_name, "")
    .map(|interaction| {
      InteractionResponse {
        plugin_configuration: Some(PluginConfiguration {
          interaction_configuration: Some(to_proto_struct(hashmap!{
            "message".to_string() => Value::String(message_name.to_string()),
            "descriptorKey".to_string() => Value::String(descriptor_hash.to_string())
          })),
          pact_configuration: None
        }),
        .. interaction
      }
    })
}

/// Constructs an interaction for the given Protobuf message descriptor
fn construct_protobuf_interaction_for_message(
  message_descriptor: &DescriptorProto,
  config: BTreeMap<String, prost_types::Value>,
  message_name: &str,
  message_part: &str
) -> anyhow::Result<InteractionResponse> {
  trace!("construct_protobuf_interaction_for_message(_, _, {}, {})", message_name, message_part);
  let mut message_builder = MessageBuilder::new(message_descriptor, message_name);
  let mut matching_rules = MatchingRuleCategory::empty("body");
  let mut generators = hashmap!{};

  debug!("Building message {} from Protobuf descriptor", message_name);
  let mut path = DocPath::root();
  if !message_part.is_empty() {
    path.push_field(message_part);
  }
  for (key, value) in &config {
    if !key.starts_with("pact:") {
      construct_message_field(message_descriptor, message_name, &mut message_builder,
                              &mut matching_rules, &mut generators, key, value, &path)?;
    }
  }

  debug!("Returning response");

  let rules = matching_rules.rules.iter().map(|(path, rule_list)| {
    (path.to_string(), MatchingRules {
      rule: rule_list.rules.iter().map(|rule| {
        let rule_values = rule.values();
        let values = if rule_values.is_empty() {
          None
        } else {
          Some(to_proto_struct(rule_values.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()))
        };
        MatchingRule {
          r#type: rule.name(),
          values
        }
      }).collect()
    })
  }).collect();

  let generators = generators.iter().map(|(path, generator)| {
    let gen_values = generator.values();
    let values = if gen_values.is_empty() {
      None
    } else {
      Some(to_proto_struct(gen_values.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()))
    };
    (path.to_string(), pact_plugin_driver::proto::Generator {
      r#type: generator.name(),
      values
    })
  }).collect();

  Ok(InteractionResponse {
    contents: Some(Body {
      content_type: format!("application/protobuf;message={}", message_name),
      content: Some(message_builder.encode_message()?.to_vec()),
      content_type_hint: ContentTypeHint::Binary as i32,
    }),
    rules,
    generators,
    interaction_markup: message_builder.generate_markup("")?,
    interaction_markup_type: MarkupType::CommonMark as i32,
    part_name: message_part.to_string(),
    .. InteractionResponse::default()
  })
}

fn construct_message_field(
  message_descriptor: &DescriptorProto,
  message_name: &str,
  message_builder: &mut MessageBuilder,
  mut matching_rules: &mut MatchingRuleCategory,
  mut generators: &mut HashMap<String, Generator>,
  key: &String,
  value: &prost_types::Value,
  path: &DocPath
) -> anyhow::Result<()> {
  trace!("construct_message_field(_, {}, {:?}, {:?}, {:?}, {}, _, {})",
    message_name, message_builder, matching_rules, generators, key, path);
  if !key.starts_with("pact:") {
    if let Some(field) = message_descriptor.field.iter().find(|f| f.name.clone().unwrap_or_default() == key.as_str()) {
      match field.r#type {
        Some(r#type) => if r#type == field_descriptor_proto::Type::Message as i32 {
          let (message_value, additional_values) = build_message_field_value(message_descriptor,
             path, field, key.as_str(), value, &mut matching_rules, &mut generators)?;
          debug!("Setting field {} to value {:?}", key, message_value);
          if field.label.unwrap_or_default() == field_descriptor_proto::Label::Repeated as i32 {
            message_builder.add_repeated_field_value(field, key.as_str(), message_value);
            for item in additional_values {
              message_builder.add_repeated_field_value(field, key.as_str(), item);
            }
          } else {
            message_builder.set_field(field, key.as_str(), message_value);
          }
        } else {
          let field_value = build_field_value(path, field, key.as_str(), value, &mut matching_rules, &mut generators)?;
          if let Some(field_value) = field_value {
            debug!("Setting field {:?} to value {:?}", key, field_value);
            message_builder.set_field(field, key.as_str(), field_value);
          }
        }
        None => {
          return Err(anyhow!("Message {} field {} is of an unknown type", message_name, key))
        }
      }
    } else {
      return Err(anyhow!("Message {} has no field {}", message_name, key))
    }
  }
  Ok(())
}

/// Constructs the field value for a field in a message.
fn build_message_field_value(
  message_descriptor: &DescriptorProto,
  path: &DocPath,
  descriptor: &FieldDescriptorProto,
  field: &str,
  value: &prost_types::Value,
  matching_rules: &mut MatchingRuleCategory,
  generators: &mut HashMap<String, Generator>
) -> anyhow::Result<(MessageFieldValue, Vec<MessageFieldValue>)> {
  trace!("build_message_field_value(_, {}, _, {}, _, {:?}, {:?})", path, field, matching_rules, generators);
  if let Some(val) = &value.kind {
    if let prost_types::value::Kind::StructValue(s) = val {
      let nested_type = find_nested_type(message_descriptor, descriptor)
        .ok_or_else(|| anyhow!("Did not find the nested type for field '{}'", field))?;
      let message_name = nested_type.name.clone().unwrap_or_else(|| "Unknown".to_string());
      let mut builder = MessageBuilder::new(&nested_type, message_name.as_str());

      if is_repeated(descriptor) {
        //todo!()
      } else {
        for (k, v) in &s.fields {
          let mut path = path.clone();
          path.push_field(k);
          construct_message_field(&nested_type, message_name.as_str(),
            &mut builder, matching_rules, generators, k, v, &path)?;
        }
      }

      Ok((MessageFieldValue {
        name: field.to_string(),
        raw_value: None,
        rtype: RType::Message(Box::new(builder))
      }, vec![]))
    } else {
      Err(anyhow!("Message field '{}' must be configured with a map structure", field))
    }
  } else {
    Err(anyhow!("Field '{}' has an unknown type, can not do anything with it", field))
  }
}

/// Constructs a simple message field (non-repeated or map) from the configuration value and
/// updates the matching rules and generators for it.
fn build_field_value(
  path: &DocPath,
  descriptor: &FieldDescriptorProto,
  key: &str,
  value: &prost_types::Value,
  matching_rules: &mut MatchingRuleCategory,
  generators: &mut HashMap<String, Generator>
) -> anyhow::Result<Option<MessageFieldValue>> {
  trace!("build_field_value({}, {}, {:?})", path, key, proto_value_to_json(&value));

  if let Some(val) = &value.kind {
    if let prost_types::value::Kind::NullValue(_) = val {
      Ok(None)
    } else {
      let mrd = parse_matcher_def(proto_value_to_string(&value)
        .ok_or_else(|| anyhow!("Field values must be a string, got {:?}", proto_type_name(value)))?.as_str())?;
      let mut field_path = path.clone();
      field_path.push_field(key);
      if !mrd.rules.is_empty() {
        for rule in &mrd.rules {
          match rule {
            Either::Left(rule) => matching_rules.add_rule(field_path.clone(), rule.clone(), RuleLogic::And),
            Either::Right(mr) => return Err(anyhow!("Was expecting a value for '{}', but got a matching reference {:?}", field_path, mr))
          }
        }
      }
      if let Some(generator) = mrd.generator {
        generators.insert(field_path.to_string(), generator);
      }
      value_for_type(key, mrd.value.as_str(), descriptor)
        .map(Some)
    }
  } else {
    Err(anyhow!("Field '{}' has an unknown type, can not do anything with it", key))
  }
}

fn value_for_type(field_name: &str, field_value: &str, descriptor: &FieldDescriptorProto) -> anyhow::Result<MessageFieldValue> {
  trace!("value_for_type({}, {}, _)", field_name, field_value);
  debug!("Creating value for type {:?} from '{}'", descriptor.type_name, field_value);
  //         Descriptors.FieldDescriptor.JavaType.ENUM -> field.enumType.findValueByName(fieldValue)
  //         Descriptors.FieldDescriptor.JavaType.MESSAGE -> {
  //           if (field.messageType.fullName == "google.protobuf.BytesValue") {
  //             BytesValue.newBuilder().setValue(ByteString.copyFromUtf8(fieldValue ?: "")).build()
  //           } else {
  //             logger.error { "field ${field.name} is a Message type" }
  //             throw RuntimeException("field ${field.name} is a Message type")
  //           }
  //         }
  let t = descriptor.r#type();
  match t {
    Type::Double => MessageFieldValue::double(field_name, field_value),
    Type::Float => MessageFieldValue::float(field_name, field_value),
    Type::Int64 | Type::Sfixed64 | Type::Sint64 => MessageFieldValue::integer_64(field_name, field_value),
    Type::Uint64 | Type::Fixed64 => MessageFieldValue::uinteger_64(field_name, field_value),
    Type::Int32 | Type::Sfixed32 | Type::Sint32 => MessageFieldValue::integer_32(field_name, field_value),
    Type::Uint32 | Type::Fixed32 => MessageFieldValue::uinteger_32(field_name, field_value),
    Type::Bool => MessageFieldValue::boolean(field_name, field_value),
    Type::String => Ok(MessageFieldValue::string(field_name, field_value)),
    // Type::Message => {}
    Type::Bytes => Ok(MessageFieldValue::bytes(field_name, field_value)),
    // Type::Enum => {}
    _ => Err(anyhow!("Protobuf field {} has an unsupported type {:?}", field_name, t))
  }
}

#[cfg(test)]
mod tests {
  use expectest::prelude::*;
  use maplit::{btreemap, hashmap};
  use pact_plugin_driver::proto::{MatchingRules, MatchingRule};
  use pact_plugin_driver::proto::interaction_response::MarkupType;
  use prost_types::{DescriptorProto, field_descriptor_proto, FieldDescriptorProto};
  use prost_types::field_descriptor_proto::Type;
  use trim_margin::MarginTrimmable;

  use crate::message_builder::RType;
  use crate::protobuf::{construct_protobuf_interaction_for_message, value_for_type};

  #[test]
  fn value_for_type_test() {
    let descriptor = FieldDescriptorProto {
      name: None,
      number: None,
      label: None,
      r#type: Some(Type::String as i32),
      type_name: Some("test".to_string()),
      extendee: None,
      default_value: None,
      oneof_index: None,
      json_name: None,
      options: None,
      proto3_optional: None
    };
    let result = value_for_type("test", "test", &descriptor).unwrap();
    expect!(result.name).to(be_equal_to("test"));
    expect!(result.raw_value).to(be_some().value("test".to_string()));
    expect!(result.rtype).to(be_equal_to(RType::String("test".to_string())));

    let descriptor = FieldDescriptorProto {
      name: None,
      number: None,
      label: None,
      r#type: Some(Type::Uint64 as i32),
      type_name: Some("uint64".to_string()),
      extendee: None,
      default_value: None,
      oneof_index: None,
      json_name: None,
      options: None,
      proto3_optional: None
    };
    let result = value_for_type("test", "100", &descriptor).unwrap();
    expect!(result.name).to(be_equal_to("test"));
    expect!(result.raw_value).to(be_some().value("100".to_string()));
    expect!(result.rtype).to(be_equal_to(RType::UInteger64(100)));
  }

  #[test]
  fn construct_protobuf_interaction_for_message_test() {
    let message_descriptor = DescriptorProto {
      name: Some("test_message".to_string()),
      field: vec![
        FieldDescriptorProto {
          name: Some("implementation".to_string()),
          number: Some(1),
          label: None,
          r#type: Some(field_descriptor_proto::Type::String as i32),
          type_name: Some("string".to_string()),
          extendee: None,
          default_value: None,
          oneof_index: None,
          json_name: None,
          options: None,
          proto3_optional: None
        },
        FieldDescriptorProto {
          name: Some("version".to_string()),
          number: Some(2),
          label: None,
          r#type: Some(field_descriptor_proto::Type::String as i32),
          type_name: Some("string".to_string()),
          extendee: None,
          default_value: None,
          oneof_index: None,
          json_name: None,
          options: None,
          proto3_optional: None
        },
        FieldDescriptorProto {
          name: Some("length".to_string()),
          number: Some(3),
          label: None,
          r#type: Some(field_descriptor_proto::Type::Int64 as i32),
          type_name: Some("int64".to_string()),
          extendee: None,
          default_value: None,
          oneof_index: None,
          json_name: None,
          options: None,
          proto3_optional: None
        },
        FieldDescriptorProto {
          name: Some("hash".to_string()),
          number: Some(4),
          label: None,
          r#type: Some(field_descriptor_proto::Type::Uint64 as i32),
          type_name: Some("uint64".to_string()),
          extendee: None,
          default_value: None,
          oneof_index: None,
          json_name: None,
          options: None,
          proto3_optional: None
        }
      ],
      extension: vec![],
      nested_type: vec![],
      enum_type: vec![],
      extension_range: vec![],
      oneof_decl: vec![],
      options: None,
      reserved_range: vec![],
      reserved_name: vec![]
    };
    let config = btreemap! {
      "implementation".to_string() => prost_types::Value { kind: Some(prost_types::value::Kind::StringValue("notEmpty('plugin-driver-rust')".to_string())) },
      "version".to_string() => prost_types::Value { kind: Some(prost_types::value::Kind::StringValue("matching(semver, '0.0.0')".to_string())) },
      "hash".to_string() => prost_types::Value { kind: Some(prost_types::value::Kind::StringValue("matching(integer, 1234)".to_string())) }
    };

    let result = construct_protobuf_interaction_for_message(&message_descriptor, config, "test_message", "").unwrap();

    let body = result.contents.as_ref().unwrap();
    expect!(body.content_type.as_str()).to(be_equal_to("application/protobuf;message=test_message"));
    expect!(body.content_type_hint).to(be_equal_to(2));
    expect!(body.content.as_ref()).to(be_some().value(&vec![
      10, // field 1 length encoded (1 << 3 + 2 == 10)
      18, // 18 bytes
      112, 108, 117, 103, 105, 110, 45, 100, 114, 105, 118, 101, 114, 45, 114, 117, 115, 116,
      18, // field 2 length encoded (2 << 3 + 2 == 18)
      5, // 5 bytes
      48, 46, 48, 46, 48,
      32, // field 4 varint encoded (4 << 3 + 0 == 32)
      210, 9 // 9 << 7 + 210 == 1234
    ]));

    expect!(result.rules).to(be_equal_to(hashmap! {
      "$.implementation".to_string() => MatchingRules { rule: vec![ MatchingRule { r#type: "not-empty".to_string(), .. MatchingRule::default() } ] },
      "$.version".to_string() => MatchingRules { rule: vec![ MatchingRule { r#type: "semver".to_string(), .. MatchingRule::default() } ] },
      "$.hash".to_string() => MatchingRules { rule: vec![ MatchingRule { r#type: "integer".to_string(), .. MatchingRule::default() } ] }
    }));

    expect!(result.generators).to(be_equal_to(hashmap! {}));

    expect!(result.interaction_markup_type).to(be_equal_to(MarkupType::CommonMark as i32));
    expect!(result.interaction_markup).to(be_equal_to(
     "|```protobuf
      |message test_message {
      |    string implementation = 1;
      |    string version = 2;
      |    uint64 hash = 4;
      |}
      |```
      |".trim_margin().unwrap()));
  }
}
