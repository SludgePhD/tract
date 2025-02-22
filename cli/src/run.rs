use crate::CliResult;
use crate::{Model, Parameters};
use ansi_term::Color::*;
use tract_hir::internal::*;
#[cfg(feature = "pulse")]
use tract_pulse::internal::*;

pub fn handle(
    params: &Parameters,
    matches: &clap::ArgMatches,
    sub_matches: &clap::ArgMatches,
) -> CliResult<()> {
    let dump = sub_matches.is_present("dump");
    #[cfg(feature = "pulse")]
    let outputs = if let Some(pulse) = params.tract_model.downcast_ref::<PulsedModel>() {
        run_pulse_t(pulse, &params)?
    } else {
        dispatch_model!(&*params.tract_model, |m| run_regular(m, &params, matches, sub_matches))?
    };

    #[cfg(not(feature = "pulse"))]
    let outputs = dispatch_model!(&*params.tract_model, |m| run_regular(m, &params, options))?;

    if dump {
        for (ix, output) in outputs.iter().enumerate() {
            println!("output #{}\n{}\n", ix, output.dump(true)?);
        }
    }

    if let Some(count) = sub_matches.value_of("assert-output-count") {
        let count = count.parse::<usize>()?;
        if count != outputs.len() {
            bail!(
                "Wrong number of outputs, command line expected {}, found {:?}",
                count,
                outputs.len()
            );
        }
    }

    crate::utils::check_outputs(&*outputs, &params.assertions.assert_outputs)?;

    if let Some(facts) = &params.assertions.assert_output_facts {
        let outputs: Vec<InferenceFact> =
            outputs.iter().map(|t| t.datum_type().fact(t.shape()).into()).collect();
        crate::utils::check_inferred(&*outputs, &*facts)?;
    }

    if let Some(asserts) = &params.assertions.assert_op_count {
        for (name, expected) in asserts {
            let count = crate::utils::count_op(&*params.tract_model, name)?;
            if count != *expected {
                bail!("Wrong number of {} operators: expected {}, got {}", name, expected, count);
            }
        }
    }

    Ok(())
}

fn run_regular(
    tract: &dyn Model,
    params: &Parameters,
    matches: &clap::ArgMatches,
    sub_matches: &clap::ArgMatches,
) -> CliResult<TVec<Arc<Tensor>>> {
    let steps = sub_matches.is_present("steps");
    let assert_sane_floats = sub_matches.is_present("assert-sane-floats");
    let mut npz = if let Some(npz) = sub_matches.value_of("save-steps") {
        let npz = std::fs::File::create(npz).with_context(|| format!("Creating {}", npz))?;
        Some(ndarray_npy::NpzWriter::new_compressed(npz))
    } else {
        None
    };
    dispatch_model!(tract, |m| {
        let plan = SimplePlan::new(m)?;
        let mut state = SimpleState::new(plan)?;
        if let Some(set) = sub_matches.values_of("set") {
            for set in set {
                let mut tokens = set.split("=");
                let sym = tokens.next().context("--set expect S=12 form")?;
                let value = tokens.next().context("--set expect S=12 form")?;
                let sym = Symbol::from(sym.chars().next().unwrap());
                let value: i64 = value.parse().context("Can not parse symbol value in set")?;
                state.session_state.resolved_symbols =
                    state.session_state.resolved_symbols.with(sym, value);
            }
        }
        let allow_random_input = matches.is_present("allow-random-input");
        let mut results = tvec!();
        let inputs = crate::tensor::retrieve_or_make_inputs(tract, params, allow_random_input)?;
        let multiturn = inputs.len() > 1;
        for (turn, inputs) in inputs.into_iter().enumerate() {
            results = state.run_plan_with_eval(inputs, |session_state, state, node, input| {
                if steps {
                    for (ix, i) in input.iter().enumerate() {
                        eprintln!(
                            "{} {}{}{:?}",
                            White.bold().paint(node.to_string()),
                            ix,
                            Blue.bold().paint("<< "),
                            i
                        );
                    }
                }
                let r = tract_core::plan::eval(session_state, state, node, input)?;
                if steps {
                    for (ix, o) in r.iter().enumerate() {
                        eprintln!(
                            "{} {}{}{:?}",
                            White.bold().paint(node.to_string()),
                            ix,
                            Yellow.bold().paint(">> "),
                            o
                        );
                    }
                }
                if let Some(npz) = npz.as_mut() {
                    for (ix, t) in r.iter().enumerate() {
                        let mut name = if ix == 0 {
                            node.name.to_string()
                        } else {
                            format!("{}:{}", node.name, ix)
                        };
                        if multiturn {
                            name = format!("turn_{}/{}", turn, name);
                        }
                        match t.datum_type() {
                            DatumType::F32 => npz.add_array(name, &t.to_array_view::<f32>()?)?,
                            DatumType::F64 => npz.add_array(name, &t.to_array_view::<f64>()?)?,
                            DatumType::I32 => npz.add_array(name, &t.to_array_view::<i32>()?)?,
                            DatumType::I8 => npz.add_array(name, &t.to_array_view::<i8>()?)?,
                            DatumType::U8 => npz.add_array(name, &t.to_array_view::<u8>()?)?,
                            DatumType::QI8(_) => unsafe {
                                npz.add_array(name, &t.to_array_view_unchecked::<i8>())?
                            },
                            DatumType::QU8(_) => unsafe {
                                npz.add_array(name, &t.to_array_view_unchecked::<u8>())?
                            },
                            DatumType::QI32(_) => unsafe {
                                npz.add_array(name, &t.to_array_view_unchecked::<i32>())?
                            },
                            _ => warn!("Not writing {}, {:?}, unsupported type", name, t),
                        }
                    }
                }
                if assert_sane_floats {
                    for (ix, o) in r.iter().enumerate() {
                        if let Ok(floats) = o.as_slice::<f32>() {
                            if let Some(pos) = floats.iter().position(|f| !f.is_finite()) {
                                eprintln!("{:?}", floats);
                                tract_core::anyhow::bail!(
                                    "Found {} in output {} of {}",
                                    floats[pos],
                                    ix,
                                    node
                                );
                            }
                        }
                    }
                }
                Ok(r)
            })?;
        }
        Ok(results)
    })
}

#[cfg(feature = "pulse")]
fn run_pulse_t(model: &PulsedModel, params: &Parameters) -> CliResult<TVec<Arc<Tensor>>> {
    let input_fact = model.input_fact(0)?;
    let output_fact = model.output_fact(0)?;

    let output_pulse = output_fact.pulse();
    //    println!("output_fact: {:?}", output_fact);
    let axis = input_fact.axis;
    let name = model.node_name(model.input_outlets()?[0].node);
    let input: &Tensor = &params.input_values.get(name).unwrap()[0];
    //    println!("input_shape: {:?}", input.shape());
    let input_dim = input.shape()[axis];
    //    println!("output_fact: {:?}", output_fact);
    let output_dim = output_fact
        .dim
        .eval(&SymbolValues::default().with(stream_symbol(), input_dim as i64))
        .to_usize()?;
    let mut output_shape = output_fact.shape.to_vec();
    output_shape[output_fact.axis] =
        (output_dim as usize + output_fact.delay + 4 * output_fact.pulse()).to_dim();
    let output_shape: TVec<usize> = output_shape.iter().map(|d| d.to_usize().unwrap()).collect();
    let plan = SimplePlan::new(model)?;
    let mut state = ::tract_core::plan::SimpleState::new(&plan)?;
    //    println!("output_shape: {:?}", output_shape);
    let pulse = input_fact.pulse();
    let mut result = tract_ndarray::ArrayD::<f32>::default(&*output_shape);
    let input = input.to_array_view::<f32>()?;
    for ix in 0..input_dim.divceil(pulse) {
        let chunk =
            input.slice_axis(tract_ndarray::Axis(axis), (ix * pulse..(ix + 1) * pulse).into());
        let input = if chunk.shape()[input_fact.axis] < pulse {
            let mut chunk_shape = chunk.shape().to_vec();
            chunk_shape[input_fact.axis] = pulse;
            let mut padded_chunk = tract_ndarray::ArrayD::<f32>::default(chunk_shape);
            padded_chunk
                .slice_axis_mut(
                    tract_ndarray::Axis(input_fact.axis),
                    (..chunk.shape()[input_fact.axis]).into(),
                )
                .assign(&chunk);
            padded_chunk
        } else {
            chunk.to_owned()
        };
        let outputs = state.run(tvec!(input.into()))?;
        let result_chunk = outputs[0].to_array_view::<f32>()?;
        result
            .slice_axis_mut(
                tract_ndarray::Axis(output_fact.axis),
                ((output_pulse * ix)..(output_pulse * (ix + 1))).into(),
            )
            .assign(&result_chunk);
    }
    result.slice_axis_inplace(tract_ndarray::Axis(output_fact.axis), (output_fact.delay..).into());
    result
        .slice_axis_inplace(tract_ndarray::Axis(output_fact.axis), (..output_dim as usize).into());
    Ok(tvec!(result.into_arc_tensor()))
}
