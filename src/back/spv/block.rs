//! Implementations for `BlockContext` methods.

use super::{
    index::{BoundsCheckResult, ExpressionPointer},
    make_local, Block, BlockContext, Dimension, Error, Instruction, LocalType, LookupType,
    LoopContext, ResultMember, Writer, WriterFlags,
};
use crate::{arena::Handle, proc::TypeResolution};
use spirv::Word;

fn get_dimension(type_inner: &crate::TypeInner) -> Dimension {
    match *type_inner {
        crate::TypeInner::Scalar { .. } => Dimension::Scalar,
        crate::TypeInner::Vector { .. } => Dimension::Vector,
        crate::TypeInner::Matrix { .. } => Dimension::Matrix,
        _ => unreachable!(),
    }
}

impl Writer {
    fn write_entry_point_return(
        &mut self,
        value_id: Word,
        ir_result: &crate::FunctionResult,
        result_members: &[ResultMember],
        body: &mut Vec<Instruction>,
    ) -> Result<(), Error> {
        for (index, res_member) in result_members.iter().enumerate() {
            let member_value_id = match ir_result.binding {
                Some(_) => value_id,
                None => {
                    let member_value_id = self.id_gen.next();
                    body.push(Instruction::composite_extract(
                        res_member.type_id,
                        member_value_id,
                        value_id,
                        &[index as u32],
                    ));
                    member_value_id
                }
            };

            body.push(Instruction::store(res_member.id, member_value_id, None));

            // Flip Y coordinate to adjust for coordinate space difference
            // between SPIR-V and our IR.
            if self.flags.contains(WriterFlags::ADJUST_COORDINATE_SPACE)
                && res_member.built_in == Some(crate::BuiltIn::Position)
            {
                let access_id = self.id_gen.next();
                let float_ptr_type_id = self.get_type_id(LookupType::Local(LocalType::Value {
                    vector_size: None,
                    kind: crate::ScalarKind::Float,
                    width: 4,
                    pointer_class: Some(spirv::StorageClass::Output),
                }))?;
                let index_y_id = self.get_index_constant(1)?;
                body.push(Instruction::access_chain(
                    float_ptr_type_id,
                    access_id,
                    res_member.id,
                    &[index_y_id],
                ));

                let load_id = self.id_gen.next();
                let float_type_id = self.get_type_id(LookupType::Local(LocalType::Value {
                    vector_size: None,
                    kind: crate::ScalarKind::Float,
                    width: 4,
                    pointer_class: None,
                }))?;
                body.push(Instruction::load(float_type_id, load_id, access_id, None));

                let neg_id = self.id_gen.next();
                body.push(Instruction::unary(
                    spirv::Op::FNegate,
                    float_type_id,
                    neg_id,
                    load_id,
                ));
                body.push(Instruction::store(access_id, neg_id, None));
            }
        }
        Ok(())
    }
}

impl<'w> BlockContext<'w> {
    fn get_type_id(&mut self, lookup_type: LookupType) -> Result<Word, Error> {
        self.writer.get_type_id(lookup_type)
    }

    /// Extend texture coordinates with an array index, if necessary.
    ///
    /// SPIR-V image read and write instructions take the coordinates of the
    /// texel to access as a vector. If the image is arrayed, the array index
    /// must be supplied as the final component of the coordinate vector.
    ///
    /// If `array_index` is `Some(expr)`, then this function constructs a new
    /// vector that is `coordinates` with `array_index` concatenated onto the
    /// end: a `vec2` becomes a `vec3`, a scalar becomes a `vec2`, and so on.
    ///
    /// If `array_index` is `None`, this function simply returns the id for
    /// `coordinates`.
    fn write_texture_coordinates(
        &mut self,
        coordinates: Handle<crate::Expression>,
        array_index: Option<Handle<crate::Expression>>,
        block: &mut Block,
    ) -> Result<Word, Error> {
        let coordinate_id = self.cached[coordinates];

        Ok(if let Some(array_index) = array_index {
            let coordinate_scalar_type_id =
                self.get_type_id(LookupType::Local(LocalType::Value {
                    vector_size: None,
                    kind: crate::ScalarKind::Float,
                    width: 4,
                    pointer_class: None,
                }))?;

            let mut constituent_ids = [0u32; 4];
            let size = match *self.fun_info[coordinates]
                .ty
                .inner_with(&self.ir_module.types)
            {
                crate::TypeInner::Scalar { .. } => {
                    constituent_ids[0] = coordinate_id;
                    crate::VectorSize::Bi
                }
                crate::TypeInner::Vector { size, .. } => {
                    for i in 0..size as u32 {
                        let id = self.gen_id();
                        constituent_ids[i as usize] = id;
                        block.body.push(Instruction::composite_extract(
                            coordinate_scalar_type_id,
                            id,
                            coordinate_id,
                            &[i],
                        ));
                    }
                    match size {
                        crate::VectorSize::Bi => crate::VectorSize::Tri,
                        crate::VectorSize::Tri => crate::VectorSize::Quad,
                        crate::VectorSize::Quad => {
                            return Err(Error::Validation("extending vec4 coordinate"));
                        }
                    }
                }
                ref other => {
                    log::error!("wrong coordinate type {:?}", other);
                    return Err(Error::Validation("coordinate type"));
                }
            };

            let array_index_f32_id = self.gen_id();
            constituent_ids[size as usize - 1] = array_index_f32_id;

            let array_index_u32_id = self.cached[array_index];
            let cast_instruction = Instruction::unary(
                spirv::Op::ConvertUToF,
                coordinate_scalar_type_id,
                array_index_f32_id,
                array_index_u32_id,
            );
            block.body.push(cast_instruction);

            let extended_coordinate_type_id =
                self.get_type_id(LookupType::Local(LocalType::Value {
                    vector_size: Some(size),
                    kind: crate::ScalarKind::Float,
                    width: 4,
                    pointer_class: None,
                }))?;

            let id = self.gen_id();
            block.body.push(Instruction::composite_construct(
                extended_coordinate_type_id,
                id,
                &constituent_ids[..size as usize],
            ));
            id
        } else {
            coordinate_id
        })
    }

    /// Decide whether to put off emitting instructions for `expr_handle`.
    ///
    /// We would like to gather together chains of `Access` and `AccessIndex`
    /// Naga expressions into a single `OpAccessChain` SPIR-V instruction. To do
    /// this, we don't generate instructions for these exprs when we first
    /// encounter them. Their ids in `self.writer.cached.ids` are left as zero. Then,
    /// once we encounter a `Load` or `Store` expression that actually needs the
    /// chain's value, we call `write_expression_pointer` to handle the whole
    /// thing in one fell swoop.
    fn is_intermediate(&self, expr_handle: Handle<crate::Expression>) -> bool {
        match self.ir_function.expressions[expr_handle] {
            crate::Expression::GlobalVariable(_) | crate::Expression::LocalVariable(_) => true,
            crate::Expression::FunctionArgument(index) => {
                let arg = &self.ir_function.arguments[index as usize];
                self.ir_module.types[arg.ty].inner.pointer_class().is_some()
            }

            // The chain rule: if this `Access...`'s `base` operand was
            // previously omitted, then omit this one, too.
            _ => self.cached.ids[expr_handle.index()] == 0,
        }
    }

    /// Cache an expression for a value.
    pub(super) fn cache_expression_value(
        &mut self,
        expr_handle: Handle<crate::Expression>,
        block: &mut Block,
    ) -> Result<(), Error> {
        let result_type_id = self.get_expression_type_id(&self.fun_info[expr_handle].ty)?;

        let id = match self.ir_function.expressions[expr_handle] {
            crate::Expression::Access { base, index: _ } if self.is_intermediate(base) => {
                // See `is_intermediate`; we'll handle this later in
                // `write_expression_pointer`.
                0
            }
            crate::Expression::Access { base, index } => {
                let base_ty = self.fun_info[base].ty.inner_with(&self.ir_module.types);
                match *base_ty {
                    crate::TypeInner::Vector { .. } => (),
                    ref other => {
                        log::error!(
                            "Unable to access base {:?} of type {:?}",
                            self.ir_function.expressions[base],
                            other
                        );
                        return Err(Error::Validation(
                            "only vectors may be dynamically indexed by value",
                        ));
                    }
                };

                self.write_vector_access(expr_handle, base, index, block)?
            }
            crate::Expression::AccessIndex { base, index: _ } if self.is_intermediate(base) => {
                // See `is_intermediate`; we'll handle this later in
                // `write_expression_pointer`.
                0
            }
            crate::Expression::AccessIndex { base, index } => {
                match *self.fun_info[base].ty.inner_with(&self.ir_module.types) {
                    crate::TypeInner::Vector { .. }
                    | crate::TypeInner::Matrix { .. }
                    | crate::TypeInner::Array { .. }
                    | crate::TypeInner::Struct { .. } => {
                        let id = self.gen_id();
                        let base_id = self.cached[base];
                        block.body.push(Instruction::composite_extract(
                            result_type_id,
                            id,
                            base_id,
                            &[index],
                        ));
                        id
                    }
                    ref other => {
                        log::error!("Unable to access index of {:?}", other);
                        return Err(Error::FeatureNotImplemented("access index for type"));
                    }
                }
            }
            crate::Expression::GlobalVariable(handle) => {
                self.writer.global_variables[handle.index()].id
            }
            crate::Expression::Constant(handle) => self.writer.constant_ids[handle.index()],
            crate::Expression::Splat { size, value } => {
                let value_id = self.cached[value];
                self.temp_list.clear();
                self.temp_list.resize(size as usize, value_id);

                let id = self.gen_id();
                block.body.push(Instruction::composite_construct(
                    result_type_id,
                    id,
                    &self.temp_list,
                ));
                id
            }
            crate::Expression::Swizzle {
                size,
                vector,
                pattern,
            } => {
                let vector_id = self.cached[vector];
                self.temp_list.clear();
                for &sc in pattern[..size as usize].iter() {
                    self.temp_list.push(sc as Word);
                }
                let id = self.gen_id();
                block.body.push(Instruction::vector_shuffle(
                    result_type_id,
                    id,
                    vector_id,
                    vector_id,
                    &self.temp_list,
                ));
                id
            }
            crate::Expression::Compose {
                ty: _,
                ref components,
            } => {
                self.temp_list.clear();
                for &component in components {
                    self.temp_list.push(self.cached[component]);
                }

                let id = self.gen_id();
                block.body.push(Instruction::composite_construct(
                    result_type_id,
                    id,
                    &self.temp_list,
                ));
                id
            }
            crate::Expression::Unary { op, expr } => {
                let id = self.gen_id();
                let expr_id = self.cached[expr];
                let expr_ty_inner = self.fun_info[expr].ty.inner_with(&self.ir_module.types);

                let spirv_op = match op {
                    crate::UnaryOperator::Negate => match expr_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Float) => spirv::Op::FNegate,
                        Some(crate::ScalarKind::Sint) => spirv::Op::SNegate,
                        Some(crate::ScalarKind::Bool) => spirv::Op::LogicalNot,
                        Some(crate::ScalarKind::Uint) | None => {
                            log::error!("Unable to negate {:?}", expr_ty_inner);
                            return Err(Error::FeatureNotImplemented("negation"));
                        }
                    },
                    crate::UnaryOperator::Not => match expr_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Bool) => spirv::Op::LogicalNot,
                        _ => spirv::Op::Not,
                    },
                };

                block
                    .body
                    .push(Instruction::unary(spirv_op, result_type_id, id, expr_id));
                id
            }
            crate::Expression::Binary { op, left, right } => {
                let id = self.gen_id();
                let left_id = self.cached[left];
                let right_id = self.cached[right];

                let left_ty_inner = self.fun_info[left].ty.inner_with(&self.ir_module.types);
                let right_ty_inner = self.fun_info[right].ty.inner_with(&self.ir_module.types);

                let left_dimension = get_dimension(left_ty_inner);
                let right_dimension = get_dimension(right_ty_inner);

                let mut preserve_order = true;

                let spirv_op = match op {
                    crate::BinaryOperator::Add => match *left_ty_inner {
                        crate::TypeInner::Scalar { kind, .. }
                        | crate::TypeInner::Vector { kind, .. } => match kind {
                            crate::ScalarKind::Float => spirv::Op::FAdd,
                            _ => spirv::Op::IAdd,
                        },
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::Subtract => match *left_ty_inner {
                        crate::TypeInner::Scalar { kind, .. }
                        | crate::TypeInner::Vector { kind, .. } => match kind {
                            crate::ScalarKind::Float => spirv::Op::FSub,
                            _ => spirv::Op::ISub,
                        },
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::Multiply => match (left_dimension, right_dimension) {
                        (Dimension::Scalar, Dimension::Vector { .. }) => {
                            preserve_order = false;
                            spirv::Op::VectorTimesScalar
                        }
                        (Dimension::Vector, Dimension::Scalar { .. }) => {
                            spirv::Op::VectorTimesScalar
                        }
                        (Dimension::Vector, Dimension::Matrix) => spirv::Op::VectorTimesMatrix,
                        (Dimension::Matrix, Dimension::Scalar { .. }) => {
                            spirv::Op::MatrixTimesScalar
                        }
                        (Dimension::Matrix, Dimension::Vector) => spirv::Op::MatrixTimesVector,
                        (Dimension::Matrix, Dimension::Matrix) => spirv::Op::MatrixTimesMatrix,
                        (Dimension::Vector, Dimension::Vector)
                        | (Dimension::Scalar, Dimension::Scalar)
                            if left_ty_inner.scalar_kind() == Some(crate::ScalarKind::Float) =>
                        {
                            spirv::Op::FMul
                        }
                        (Dimension::Vector, Dimension::Vector)
                        | (Dimension::Scalar, Dimension::Scalar) => spirv::Op::IMul,
                        other => unimplemented!("Mul {:?}", other),
                    },
                    crate::BinaryOperator::Divide => match left_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Sint) => spirv::Op::SDiv,
                        Some(crate::ScalarKind::Uint) => spirv::Op::UDiv,
                        Some(crate::ScalarKind::Float) => spirv::Op::FDiv,
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::Modulo => match left_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Sint) => spirv::Op::SMod,
                        Some(crate::ScalarKind::Uint) => spirv::Op::UMod,
                        Some(crate::ScalarKind::Float) => spirv::Op::FMod,
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::Equal => match left_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Sint) | Some(crate::ScalarKind::Uint) => {
                            spirv::Op::IEqual
                        }
                        Some(crate::ScalarKind::Float) => spirv::Op::FOrdEqual,
                        Some(crate::ScalarKind::Bool) => spirv::Op::LogicalEqual,
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::NotEqual => match left_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Sint) | Some(crate::ScalarKind::Uint) => {
                            spirv::Op::INotEqual
                        }
                        Some(crate::ScalarKind::Float) => spirv::Op::FOrdNotEqual,
                        Some(crate::ScalarKind::Bool) => spirv::Op::LogicalNotEqual,
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::Less => match left_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Sint) => spirv::Op::SLessThan,
                        Some(crate::ScalarKind::Uint) => spirv::Op::ULessThan,
                        Some(crate::ScalarKind::Float) => spirv::Op::FOrdLessThan,
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::LessEqual => match left_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Sint) => spirv::Op::SLessThanEqual,
                        Some(crate::ScalarKind::Uint) => spirv::Op::ULessThanEqual,
                        Some(crate::ScalarKind::Float) => spirv::Op::FOrdLessThanEqual,
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::Greater => match left_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Sint) => spirv::Op::SGreaterThan,
                        Some(crate::ScalarKind::Uint) => spirv::Op::UGreaterThan,
                        Some(crate::ScalarKind::Float) => spirv::Op::FOrdGreaterThan,
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::GreaterEqual => match left_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Sint) => spirv::Op::SGreaterThanEqual,
                        Some(crate::ScalarKind::Uint) => spirv::Op::UGreaterThanEqual,
                        Some(crate::ScalarKind::Float) => spirv::Op::FOrdGreaterThanEqual,
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::And => spirv::Op::BitwiseAnd,
                    crate::BinaryOperator::ExclusiveOr => spirv::Op::BitwiseXor,
                    crate::BinaryOperator::InclusiveOr => spirv::Op::BitwiseOr,
                    crate::BinaryOperator::LogicalAnd => spirv::Op::LogicalAnd,
                    crate::BinaryOperator::LogicalOr => spirv::Op::LogicalOr,
                    crate::BinaryOperator::ShiftLeft => spirv::Op::ShiftLeftLogical,
                    crate::BinaryOperator::ShiftRight => match left_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Sint) => spirv::Op::ShiftRightArithmetic,
                        Some(crate::ScalarKind::Uint) => spirv::Op::ShiftRightLogical,
                        _ => unimplemented!(),
                    },
                };

                block.body.push(Instruction::binary(
                    spirv_op,
                    result_type_id,
                    id,
                    if preserve_order { left_id } else { right_id },
                    if preserve_order { right_id } else { left_id },
                ));
                id
            }
            crate::Expression::Math {
                fun,
                arg,
                arg1,
                arg2,
            } => {
                use crate::MathFunction as Mf;
                enum MathOp {
                    Ext(spirv::GLOp),
                    Custom(Instruction),
                }

                let arg0_id = self.cached[arg];
                let arg_scalar_kind = self.fun_info[arg]
                    .ty
                    .inner_with(&self.ir_module.types)
                    .scalar_kind();
                let arg1_id = match arg1 {
                    Some(handle) => self.cached[handle],
                    None => 0,
                };
                let arg2_id = match arg2 {
                    Some(handle) => self.cached[handle],
                    None => 0,
                };

                let id = self.gen_id();
                let math_op = match fun {
                    // comparison
                    Mf::Abs => {
                        match arg_scalar_kind {
                            Some(crate::ScalarKind::Float) => MathOp::Ext(spirv::GLOp::FAbs),
                            Some(crate::ScalarKind::Sint) => MathOp::Ext(spirv::GLOp::SAbs),
                            Some(crate::ScalarKind::Uint) => {
                                MathOp::Custom(Instruction::unary(
                                    spirv::Op::CopyObject, // do nothing
                                    result_type_id,
                                    id,
                                    arg0_id,
                                ))
                            }
                            other => unimplemented!("Unexpected abs({:?})", other),
                        }
                    }
                    Mf::Min => MathOp::Ext(match arg_scalar_kind {
                        Some(crate::ScalarKind::Float) => spirv::GLOp::FMin,
                        Some(crate::ScalarKind::Sint) => spirv::GLOp::SMin,
                        Some(crate::ScalarKind::Uint) => spirv::GLOp::UMin,
                        other => unimplemented!("Unexpected min({:?})", other),
                    }),
                    Mf::Max => MathOp::Ext(match arg_scalar_kind {
                        Some(crate::ScalarKind::Float) => spirv::GLOp::FMax,
                        Some(crate::ScalarKind::Sint) => spirv::GLOp::SMax,
                        Some(crate::ScalarKind::Uint) => spirv::GLOp::UMax,
                        other => unimplemented!("Unexpected max({:?})", other),
                    }),
                    Mf::Clamp => MathOp::Ext(match arg_scalar_kind {
                        Some(crate::ScalarKind::Float) => spirv::GLOp::FClamp,
                        Some(crate::ScalarKind::Sint) => spirv::GLOp::SClamp,
                        Some(crate::ScalarKind::Uint) => spirv::GLOp::UClamp,
                        other => unimplemented!("Unexpected max({:?})", other),
                    }),
                    // trigonometry
                    Mf::Sin => MathOp::Ext(spirv::GLOp::Sin),
                    Mf::Sinh => MathOp::Ext(spirv::GLOp::Sinh),
                    Mf::Asin => MathOp::Ext(spirv::GLOp::Asin),
                    Mf::Cos => MathOp::Ext(spirv::GLOp::Cos),
                    Mf::Cosh => MathOp::Ext(spirv::GLOp::Cosh),
                    Mf::Acos => MathOp::Ext(spirv::GLOp::Acos),
                    Mf::Tan => MathOp::Ext(spirv::GLOp::Tan),
                    Mf::Tanh => MathOp::Ext(spirv::GLOp::Tanh),
                    Mf::Atan => MathOp::Ext(spirv::GLOp::Atan),
                    Mf::Atan2 => MathOp::Ext(spirv::GLOp::Atan2),
                    // decomposition
                    Mf::Ceil => MathOp::Ext(spirv::GLOp::Ceil),
                    Mf::Round => MathOp::Ext(spirv::GLOp::Round),
                    Mf::Floor => MathOp::Ext(spirv::GLOp::Floor),
                    Mf::Fract => MathOp::Ext(spirv::GLOp::Fract),
                    Mf::Trunc => MathOp::Ext(spirv::GLOp::Trunc),
                    Mf::Modf => MathOp::Ext(spirv::GLOp::Modf),
                    Mf::Frexp => MathOp::Ext(spirv::GLOp::Frexp),
                    Mf::Ldexp => MathOp::Ext(spirv::GLOp::Ldexp),
                    // geometry
                    Mf::Dot => MathOp::Custom(Instruction::binary(
                        spirv::Op::Dot,
                        result_type_id,
                        id,
                        arg0_id,
                        arg1_id,
                    )),
                    Mf::Outer => MathOp::Custom(Instruction::binary(
                        spirv::Op::OuterProduct,
                        result_type_id,
                        id,
                        arg0_id,
                        arg1_id,
                    )),
                    Mf::Cross => MathOp::Ext(spirv::GLOp::Cross),
                    Mf::Distance => MathOp::Ext(spirv::GLOp::Distance),
                    Mf::Length => MathOp::Ext(spirv::GLOp::Length),
                    Mf::Normalize => MathOp::Ext(spirv::GLOp::Normalize),
                    Mf::FaceForward => MathOp::Ext(spirv::GLOp::FaceForward),
                    Mf::Reflect => MathOp::Ext(spirv::GLOp::Reflect),
                    Mf::Refract => MathOp::Ext(spirv::GLOp::Refract),
                    // exponent
                    Mf::Exp => MathOp::Ext(spirv::GLOp::Exp),
                    Mf::Exp2 => MathOp::Ext(spirv::GLOp::Exp2),
                    Mf::Log => MathOp::Ext(spirv::GLOp::Log),
                    Mf::Log2 => MathOp::Ext(spirv::GLOp::Log2),
                    Mf::Pow => MathOp::Ext(spirv::GLOp::Pow),
                    // computational
                    Mf::Sign => MathOp::Ext(match arg_scalar_kind {
                        Some(crate::ScalarKind::Float) => spirv::GLOp::FSign,
                        Some(crate::ScalarKind::Sint) => spirv::GLOp::SSign,
                        other => unimplemented!("Unexpected sign({:?})", other),
                    }),
                    Mf::Fma => MathOp::Ext(spirv::GLOp::Fma),
                    Mf::Mix => MathOp::Ext(spirv::GLOp::FMix),
                    Mf::Step => MathOp::Ext(spirv::GLOp::Step),
                    Mf::SmoothStep => MathOp::Ext(spirv::GLOp::SmoothStep),
                    Mf::Sqrt => MathOp::Ext(spirv::GLOp::Sqrt),
                    Mf::InverseSqrt => MathOp::Ext(spirv::GLOp::InverseSqrt),
                    Mf::Inverse => MathOp::Ext(spirv::GLOp::MatrixInverse),
                    Mf::Transpose => MathOp::Custom(Instruction::unary(
                        spirv::Op::Transpose,
                        result_type_id,
                        id,
                        arg0_id,
                    )),
                    Mf::Determinant => MathOp::Ext(spirv::GLOp::Determinant),
                    Mf::ReverseBits | Mf::CountOneBits => {
                        log::error!("unimplemented math function {:?}", fun);
                        return Err(Error::FeatureNotImplemented("math function"));
                    }
                };

                block.body.push(match math_op {
                    MathOp::Ext(op) => Instruction::ext_inst(
                        self.writer.gl450_ext_inst_id,
                        op,
                        result_type_id,
                        id,
                        &[arg0_id, arg1_id, arg2_id][..fun.argument_count()],
                    ),
                    MathOp::Custom(inst) => inst,
                });
                id
            }
            crate::Expression::LocalVariable(variable) => self.function.variables[&variable].id,
            crate::Expression::Load { pointer } => {
                match self.write_expression_pointer(pointer, block)? {
                    ExpressionPointer::Ready { pointer_id } => {
                        let id = self.gen_id();
                        block
                            .body
                            .push(Instruction::load(result_type_id, id, pointer_id, None));
                        id
                    }
                    ExpressionPointer::Conditional { condition, access } => {
                        self.write_conditional_indexed_load(
                            result_type_id,
                            condition,
                            block,
                            move |id_gen, block| {
                                // The in-bounds path. Perform the access and the load.
                                let pointer_id = access.result_id.unwrap();
                                let value_id = id_gen.next();
                                block.body.push(access);
                                block.body.push(Instruction::load(
                                    result_type_id,
                                    value_id,
                                    pointer_id,
                                    None,
                                ));
                                value_id
                            },
                        )
                    }
                }
            }
            crate::Expression::FunctionArgument(index) => self.function.parameter_id(index),
            crate::Expression::Call(_function) => self.writer.lookup_function_call[&expr_handle],
            crate::Expression::As {
                expr,
                kind,
                convert,
            } => {
                use crate::ScalarKind as Sk;

                let expr_id = self.cached[expr];
                let (src_kind, src_width) =
                    match *self.fun_info[expr].ty.inner_with(&self.ir_module.types) {
                        crate::TypeInner::Scalar { kind, width }
                        | crate::TypeInner::Vector {
                            kind,
                            width,
                            size: _,
                        } => (kind, width),
                        crate::TypeInner::Matrix { width, .. } => (crate::ScalarKind::Float, width),
                        ref other => {
                            log::error!("As source {:?}", other);
                            return Err(Error::Validation("Unexpected Expression::As source"));
                        }
                    };

                let op = match (src_kind, kind, convert) {
                    (_, _, None) => spirv::Op::Bitcast,
                    (Sk::Float, Sk::Uint, Some(_)) => spirv::Op::ConvertFToU,
                    (Sk::Float, Sk::Sint, Some(_)) => spirv::Op::ConvertFToS,
                    (Sk::Float, Sk::Float, Some(dst_width)) if src_width != dst_width => {
                        spirv::Op::FConvert
                    }
                    (Sk::Sint, Sk::Float, Some(_)) => spirv::Op::ConvertSToF,
                    (Sk::Sint, Sk::Sint, Some(dst_width)) if src_width != dst_width => {
                        spirv::Op::SConvert
                    }
                    (Sk::Uint, Sk::Float, Some(_)) => spirv::Op::ConvertUToF,
                    (Sk::Uint, Sk::Uint, Some(dst_width)) if src_width != dst_width => {
                        spirv::Op::UConvert
                    }
                    // We assume it's either an identity cast, or int-uint.
                    _ => spirv::Op::Bitcast,
                };

                let id = self.gen_id();
                let instruction = Instruction::unary(op, result_type_id, id, expr_id);
                block.body.push(instruction);
                id
            }
            crate::Expression::ImageLoad {
                image,
                coordinate,
                array_index,
                index,
            } => {
                let image_id = self.get_image_id(image);
                let coordinate_id =
                    self.write_texture_coordinates(coordinate, array_index, block)?;

                let id = self.gen_id();

                let image_ty = self.fun_info[image].ty.inner_with(&self.ir_module.types);
                let mut instruction = match *image_ty {
                    crate::TypeInner::Image {
                        class: crate::ImageClass::Storage { .. },
                        ..
                    } => Instruction::image_read(result_type_id, id, image_id, coordinate_id),
                    crate::TypeInner::Image {
                        class: crate::ImageClass::Depth { multi: _ },
                        ..
                    } => {
                        // Vulkan doesn't know about our `Depth` class, and it returns `vec4<f32>`,
                        // so we need to grab the first component out of it.
                        let load_result_type_id =
                            self.get_type_id(LookupType::Local(LocalType::Value {
                                vector_size: Some(crate::VectorSize::Quad),
                                kind: crate::ScalarKind::Float,
                                width: 4,
                                pointer_class: None,
                            }))?;
                        Instruction::image_fetch(load_result_type_id, id, image_id, coordinate_id)
                    }
                    _ => Instruction::image_fetch(result_type_id, id, image_id, coordinate_id),
                };

                if let Some(index) = index {
                    let index_id = self.cached[index];
                    let image_ops = match *self.fun_info[image].ty.inner_with(&self.ir_module.types)
                    {
                        crate::TypeInner::Image {
                            class: crate::ImageClass::Sampled { multi: true, .. },
                            ..
                        }
                        | crate::TypeInner::Image {
                            class: crate::ImageClass::Depth { multi: true },
                            ..
                        } => spirv::ImageOperands::SAMPLE,
                        _ => spirv::ImageOperands::LOD,
                    };
                    instruction.add_operand(image_ops.bits());
                    instruction.add_operand(index_id);
                }

                let inst_type_id = instruction.type_id;
                block.body.push(instruction);
                if inst_type_id != Some(result_type_id) {
                    let sub_id = self.gen_id();
                    block.body.push(Instruction::composite_extract(
                        result_type_id,
                        sub_id,
                        id,
                        &[0],
                    ));
                    sub_id
                } else {
                    id
                }
            }
            crate::Expression::ImageSample {
                image,
                sampler,
                coordinate,
                array_index,
                offset,
                level,
                depth_ref,
            } => {
                use super::instructions::SampleLod;
                // image
                let image_id = self.get_image_id(image);
                let image_type = self.fun_info[image].ty.handle().unwrap();
                // Vulkan doesn't know about our `Depth` class, and it returns `vec4<f32>`,
                // so we need to grab the first component out of it.
                let needs_sub_access = match self.ir_module.types[image_type].inner {
                    crate::TypeInner::Image {
                        class: crate::ImageClass::Depth { .. },
                        ..
                    } => depth_ref.is_none(),
                    _ => false,
                };
                let sample_result_type_id = if needs_sub_access {
                    self.get_type_id(LookupType::Local(LocalType::Value {
                        vector_size: Some(crate::VectorSize::Quad),
                        kind: crate::ScalarKind::Float,
                        width: 4,
                        pointer_class: None,
                    }))?
                } else {
                    result_type_id
                };

                // OpTypeSampledImage
                let image_type_id = self.get_type_id(LookupType::Handle(image_type))?;
                let sampled_image_type_id =
                    self.get_type_id(LookupType::Local(LocalType::SampledImage { image_type_id }))?;

                let sampler_id = self.get_image_id(sampler);
                let coordinate_id =
                    self.write_texture_coordinates(coordinate, array_index, block)?;

                let sampled_image_id = self.gen_id();
                block.body.push(Instruction::sampled_image(
                    sampled_image_type_id,
                    sampled_image_id,
                    image_id,
                    sampler_id,
                ));
                let id = self.gen_id();

                let depth_id = depth_ref.map(|handle| self.cached[handle]);
                let mut mask = spirv::ImageOperands::empty();
                mask.set(spirv::ImageOperands::CONST_OFFSET, offset.is_some());

                let mut main_instruction = match level {
                    crate::SampleLevel::Zero => {
                        let mut inst = Instruction::image_sample(
                            sample_result_type_id,
                            id,
                            SampleLod::Explicit,
                            sampled_image_id,
                            coordinate_id,
                            depth_id,
                        );

                        let zero_id = self
                            .writer
                            .get_constant_scalar(crate::ScalarValue::Float(0.0), 4)?;

                        mask |= spirv::ImageOperands::LOD;
                        inst.add_operand(mask.bits());
                        inst.add_operand(zero_id);

                        inst
                    }
                    crate::SampleLevel::Auto => {
                        let mut inst = Instruction::image_sample(
                            sample_result_type_id,
                            id,
                            SampleLod::Implicit,
                            sampled_image_id,
                            coordinate_id,
                            depth_id,
                        );
                        if !mask.is_empty() {
                            inst.add_operand(mask.bits());
                        }
                        inst
                    }
                    crate::SampleLevel::Exact(lod_handle) => {
                        let mut inst = Instruction::image_sample(
                            sample_result_type_id,
                            id,
                            SampleLod::Explicit,
                            sampled_image_id,
                            coordinate_id,
                            depth_id,
                        );

                        let lod_id = self.cached[lod_handle];
                        mask |= spirv::ImageOperands::LOD;
                        inst.add_operand(mask.bits());
                        inst.add_operand(lod_id);

                        inst
                    }
                    crate::SampleLevel::Bias(bias_handle) => {
                        let mut inst = Instruction::image_sample(
                            sample_result_type_id,
                            id,
                            SampleLod::Implicit,
                            sampled_image_id,
                            coordinate_id,
                            depth_id,
                        );

                        let bias_id = self.cached[bias_handle];
                        mask |= spirv::ImageOperands::BIAS;
                        inst.add_operand(mask.bits());
                        inst.add_operand(bias_id);

                        inst
                    }
                    crate::SampleLevel::Gradient { x, y } => {
                        let mut inst = Instruction::image_sample(
                            sample_result_type_id,
                            id,
                            SampleLod::Explicit,
                            sampled_image_id,
                            coordinate_id,
                            depth_id,
                        );

                        let x_id = self.cached[x];
                        let y_id = self.cached[y];
                        mask |= spirv::ImageOperands::GRAD;
                        inst.add_operand(mask.bits());
                        inst.add_operand(x_id);
                        inst.add_operand(y_id);

                        inst
                    }
                };

                if let Some(offset_const) = offset {
                    let offset_id = self.writer.constant_ids[offset_const.index()];
                    main_instruction.add_operand(offset_id);
                }

                block.body.push(main_instruction);

                if needs_sub_access {
                    let sub_id = self.gen_id();
                    block.body.push(Instruction::composite_extract(
                        result_type_id,
                        sub_id,
                        id,
                        &[0],
                    ));
                    sub_id
                } else {
                    id
                }
            }
            crate::Expression::Select {
                condition,
                accept,
                reject,
            } => {
                let id = self.gen_id();
                let mut condition_id = self.cached[condition];
                let accept_id = self.cached[accept];
                let reject_id = self.cached[reject];

                let condition_ty = self.fun_info[condition]
                    .ty
                    .inner_with(&self.ir_module.types);
                let object_ty = self.fun_info[accept].ty.inner_with(&self.ir_module.types);

                if let (
                    &crate::TypeInner::Scalar {
                        kind: crate::ScalarKind::Bool,
                        width,
                    },
                    &crate::TypeInner::Vector { size, .. },
                ) = (condition_ty, object_ty)
                {
                    self.temp_list.clear();
                    self.temp_list.resize(size as usize, condition_id);

                    let bool_vector_type_id =
                        self.get_type_id(LookupType::Local(LocalType::Value {
                            vector_size: Some(size),
                            kind: crate::ScalarKind::Bool,
                            width,
                            pointer_class: None,
                        }))?;

                    let id = self.gen_id();
                    block.body.push(Instruction::composite_construct(
                        bool_vector_type_id,
                        id,
                        &self.temp_list,
                    ));
                    condition_id = id
                }

                let instruction =
                    Instruction::select(result_type_id, id, condition_id, accept_id, reject_id);
                block.body.push(instruction);
                id
            }
            crate::Expression::Derivative { axis, expr } => {
                use crate::DerivativeAxis as Da;

                let id = self.gen_id();
                let expr_id = self.cached[expr];
                let op = match axis {
                    Da::X => spirv::Op::DPdx,
                    Da::Y => spirv::Op::DPdy,
                    Da::Width => spirv::Op::Fwidth,
                };
                block
                    .body
                    .push(Instruction::derivative(op, result_type_id, id, expr_id));
                id
            }
            crate::Expression::ImageQuery { image, query } => {
                use crate::{ImageClass as Ic, ImageDimension as Id, ImageQuery as Iq};

                let image_id = self.get_image_id(image);
                let image_type = self.fun_info[image].ty.handle().unwrap();
                let (dim, arrayed, class) = match self.ir_module.types[image_type].inner {
                    crate::TypeInner::Image {
                        dim,
                        arrayed,
                        class,
                    } => (dim, arrayed, class),
                    _ => {
                        return Err(Error::Validation("image type"));
                    }
                };

                self.writer.check(&[spirv::Capability::ImageQuery])?;

                match query {
                    Iq::Size { level } => {
                        let dim_coords = match dim {
                            Id::D1 => 1,
                            Id::D2 | Id::Cube => 2,
                            Id::D3 => 3,
                        };
                        let extended_size_type_id = {
                            let array_coords = if arrayed { 1 } else { 0 };
                            let vector_size = match dim_coords + array_coords {
                                2 => Some(crate::VectorSize::Bi),
                                3 => Some(crate::VectorSize::Tri),
                                4 => Some(crate::VectorSize::Quad),
                                _ => None,
                            };
                            self.get_type_id(LookupType::Local(LocalType::Value {
                                vector_size,
                                kind: crate::ScalarKind::Sint,
                                width: 4,
                                pointer_class: None,
                            }))?
                        };

                        let (query_op, level_id) = match class {
                            Ic::Storage { .. } => (spirv::Op::ImageQuerySize, None),
                            _ => {
                                let level_id = match level {
                                    Some(expr) => self.cached[expr],
                                    None => self.get_index_constant(0)?,
                                };
                                (spirv::Op::ImageQuerySizeLod, Some(level_id))
                            }
                        };

                        // The ID of the vector returned by SPIR-V, which contains the dimensions
                        // as well as the layer count.
                        let id_extended = self.gen_id();
                        let mut inst = Instruction::image_query(
                            query_op,
                            extended_size_type_id,
                            id_extended,
                            image_id,
                        );
                        if let Some(expr_id) = level_id {
                            inst.add_operand(expr_id);
                        }
                        block.body.push(inst);

                        if result_type_id != extended_size_type_id {
                            let id = self.gen_id();
                            let components = match dim {
                                // always pick the first component, and duplicate it for all 3 dimensions
                                Id::Cube => &[0u32, 0][..],
                                _ => &[0u32, 1, 2, 3][..dim_coords],
                            };
                            block.body.push(Instruction::vector_shuffle(
                                result_type_id,
                                id,
                                id_extended,
                                id_extended,
                                components,
                            ));
                            id
                        } else {
                            id_extended
                        }
                    }
                    Iq::NumLevels => {
                        let id = self.gen_id();
                        block.body.push(Instruction::image_query(
                            spirv::Op::ImageQueryLevels,
                            result_type_id,
                            id,
                            image_id,
                        ));
                        id
                    }
                    Iq::NumLayers => {
                        let vec_size = match dim {
                            Id::D1 => crate::VectorSize::Bi,
                            Id::D2 | Id::Cube => crate::VectorSize::Tri,
                            Id::D3 => crate::VectorSize::Quad,
                        };
                        let extended_size_type_id =
                            self.get_type_id(LookupType::Local(LocalType::Value {
                                vector_size: Some(vec_size),
                                kind: crate::ScalarKind::Sint,
                                width: 4,
                                pointer_class: None,
                            }))?;
                        let id_extended = self.gen_id();
                        let mut inst = Instruction::image_query(
                            spirv::Op::ImageQuerySizeLod,
                            extended_size_type_id,
                            id_extended,
                            image_id,
                        );
                        inst.add_operand(self.get_index_constant(0)?);
                        block.body.push(inst);
                        let id = self.gen_id();
                        block.body.push(Instruction::composite_extract(
                            result_type_id,
                            id,
                            id_extended,
                            &[vec_size as u32 - 1],
                        ));
                        id
                    }
                    Iq::NumSamples => {
                        let id = self.gen_id();
                        block.body.push(Instruction::image_query(
                            spirv::Op::ImageQuerySamples,
                            result_type_id,
                            id,
                            image_id,
                        ));
                        id
                    }
                }
            }
            crate::Expression::Relational { fun, argument } => {
                use crate::RelationalFunction as Rf;
                let arg_id = self.cached[argument];
                let op = match fun {
                    Rf::All => spirv::Op::All,
                    Rf::Any => spirv::Op::Any,
                    Rf::IsNan => spirv::Op::IsNan,
                    Rf::IsInf => spirv::Op::IsInf,
                    //TODO: these require Kernel capability
                    Rf::IsFinite | Rf::IsNormal => {
                        return Err(Error::FeatureNotImplemented("is finite/normal"))
                    }
                };
                let id = self.gen_id();
                block
                    .body
                    .push(Instruction::relational(op, result_type_id, id, arg_id));
                id
            }
            crate::Expression::ArrayLength(expr) => self.write_runtime_array_length(expr, block)?,
        };

        self.cached[expr_handle] = id;
        Ok(())
    }

    /// Build an `OpAccessChain` instruction for a left-hand-side expression.
    ///
    /// Emit any needed bounds-checking expressions to `block`.
    ///
    /// On success, the return value is an [`ExpressionPointer`] value; see the
    /// documentation for that type.
    fn write_expression_pointer(
        &mut self,
        mut expr_handle: Handle<crate::Expression>,
        block: &mut Block,
    ) -> Result<ExpressionPointer, Error> {
        let result_lookup_ty = match self.fun_info[expr_handle].ty {
            TypeResolution::Handle(ty_handle) => LookupType::Handle(ty_handle),
            TypeResolution::Value(ref inner) => LookupType::Local(make_local(inner).unwrap()),
        };
        let result_type_id = self.get_type_id(result_lookup_ty)?;

        // The id of the boolean `and` of all dynamic bounds checks up to this point. If
        // `None`, then we haven't done any dynamic bounds checks yet.
        //
        // When we have a chain of bounds checks, we combine them with `OpLogicalAnd`, not
        // a short-circuit branch. This means we might do comparisons we don't need to,
        // but we expect these checks to almost always succeed, and keeping branches to a
        // minimum is essential.
        let mut accumulated_checks = None;

        self.temp_list.clear();
        let root_id = loop {
            expr_handle = match self.ir_function.expressions[expr_handle] {
                crate::Expression::Access { base, index } => {
                    let index_id = match self.write_bounds_check(base, index, block)? {
                        BoundsCheckResult::KnownInBounds(known_index) => {
                            // Even if the index is known, `OpAccessIndex`
                            // requires expression operands, not literals.
                            let scalar = crate::ScalarValue::Uint(known_index as u64);
                            self.writer.get_constant_scalar(scalar, 4)?
                        }
                        BoundsCheckResult::Computed(computed_index_id) => computed_index_id,
                        BoundsCheckResult::Conditional(comparison_id) => {
                            match accumulated_checks {
                                Some(prior_checks) => {
                                    let combined = self.gen_id();
                                    block.body.push(Instruction::binary(
                                        spirv::Op::LogicalAnd,
                                        self.writer.get_bool_type_id()?,
                                        combined,
                                        prior_checks,
                                        comparison_id,
                                    ));
                                    accumulated_checks = Some(combined);
                                }
                                None => {
                                    // Start a fresh chain of checks.
                                    accumulated_checks = Some(comparison_id);
                                }
                            }

                            // Either way, the index to use is unchanged.
                            self.cached[index]
                        }
                    };
                    self.temp_list.push(index_id);

                    base
                }
                crate::Expression::AccessIndex { base, index } => {
                    let const_id = self.get_index_constant(index)?;
                    self.temp_list.push(const_id);
                    base
                }
                crate::Expression::GlobalVariable(handle) => {
                    let gv = &self.writer.global_variables[handle.index()];
                    break gv.id;
                }
                crate::Expression::LocalVariable(variable) => {
                    let local_var = &self.function.variables[&variable];
                    break local_var.id;
                }
                crate::Expression::FunctionArgument(index) => {
                    break self.function.parameter_id(index);
                }
                ref other => unimplemented!("Unexpected pointer expression {:?}", other),
            }
        };

        let pointer = if self.temp_list.is_empty() {
            ExpressionPointer::Ready {
                pointer_id: root_id,
            }
        } else {
            self.temp_list.reverse();
            let pointer_id = self.gen_id();
            let access =
                Instruction::access_chain(result_type_id, pointer_id, root_id, &self.temp_list);

            // If we generated some bounds checks, we need to leave it to our
            // caller to generate the branch, the access, the load or store, and
            // the zero value (for loads). Otherwise, we can emit the access
            // ourselves, and just hand them the id of the pointer.
            match accumulated_checks {
                Some(condition) => ExpressionPointer::Conditional { condition, access },
                None => {
                    block.body.push(access);
                    ExpressionPointer::Ready { pointer_id }
                }
            }
        };

        Ok(pointer)
    }

    fn get_image_id(&mut self, expr_handle: Handle<crate::Expression>) -> Word {
        let id = match self.ir_function.expressions[expr_handle] {
            crate::Expression::GlobalVariable(handle) => {
                self.writer.global_variables[handle.index()].handle_id
            }
            crate::Expression::FunctionArgument(i) => {
                self.function.parameters[i as usize].handle_id
            }
            ref other => unreachable!("Unexpected image expression {:?}", other),
        };

        if id == 0 {
            unreachable!(
                "Image expression {:?} doesn't have a handle ID",
                expr_handle
            );
        }

        id
    }

    pub(super) fn write_block(
        &mut self,
        label_id: Word,
        statements: &[crate::Statement],
        exit_id: Option<Word>,
        loop_context: LoopContext,
    ) -> Result<(), Error> {
        let mut block = Block::new(label_id);

        for statement in statements {
            match *statement {
                crate::Statement::Emit(ref range) => {
                    for handle in range.clone() {
                        self.cache_expression_value(handle, &mut block)?;
                    }
                }
                crate::Statement::Block(ref block_statements) => {
                    let scope_id = self.gen_id();
                    self.function.consume(block, Instruction::branch(scope_id));

                    let merge_id = self.gen_id();
                    self.write_block(scope_id, block_statements, Some(merge_id), loop_context)?;

                    block = Block::new(merge_id);
                }
                crate::Statement::If {
                    condition,
                    ref accept,
                    ref reject,
                } => {
                    let condition_id = self.cached[condition];

                    let merge_id = self.gen_id();
                    block.body.push(Instruction::selection_merge(
                        merge_id,
                        spirv::SelectionControl::NONE,
                    ));

                    let accept_id = if accept.is_empty() {
                        None
                    } else {
                        Some(self.gen_id())
                    };
                    let reject_id = if reject.is_empty() {
                        None
                    } else {
                        Some(self.gen_id())
                    };

                    self.function.consume(
                        block,
                        Instruction::branch_conditional(
                            condition_id,
                            accept_id.unwrap_or(merge_id),
                            reject_id.unwrap_or(merge_id),
                        ),
                    );

                    if let Some(block_id) = accept_id {
                        self.write_block(block_id, accept, Some(merge_id), loop_context)?;
                    }
                    if let Some(block_id) = reject_id {
                        self.write_block(block_id, reject, Some(merge_id), loop_context)?;
                    }

                    block = Block::new(merge_id);
                }
                crate::Statement::Switch {
                    selector,
                    ref cases,
                    ref default,
                } => {
                    let selector_id = self.cached[selector];

                    let merge_id = self.gen_id();
                    block.body.push(Instruction::selection_merge(
                        merge_id,
                        spirv::SelectionControl::NONE,
                    ));

                    let default_id = self.gen_id();
                    let raw_cases = cases
                        .iter()
                        .map(|c| super::instructions::Case {
                            value: c.value as Word,
                            label_id: self.gen_id(),
                        })
                        .collect::<Vec<_>>();

                    self.function.consume(
                        block,
                        Instruction::switch(selector_id, default_id, &raw_cases),
                    );

                    for (i, (case, raw_case)) in cases.iter().zip(raw_cases.iter()).enumerate() {
                        let case_finish_id = if case.fall_through {
                            match raw_cases.get(i + 1) {
                                Some(rc) => rc.label_id,
                                None => default_id,
                            }
                        } else {
                            merge_id
                        };
                        self.write_block(
                            raw_case.label_id,
                            &case.body,
                            Some(case_finish_id),
                            LoopContext::default(),
                        )?;
                    }

                    self.write_block(default_id, default, Some(merge_id), LoopContext::default())?;

                    block = Block::new(merge_id);
                }
                crate::Statement::Loop {
                    ref body,
                    ref continuing,
                } => {
                    let preamble_id = self.gen_id();
                    self.function
                        .consume(block, Instruction::branch(preamble_id));

                    let merge_id = self.gen_id();
                    let body_id = self.gen_id();
                    let continuing_id = self.gen_id();

                    // SPIR-V requires the continuing to the `OpLoopMerge`,
                    // so we have to start a new block with it.
                    block = Block::new(preamble_id);
                    block.body.push(Instruction::loop_merge(
                        merge_id,
                        continuing_id,
                        spirv::SelectionControl::NONE,
                    ));
                    self.function.consume(block, Instruction::branch(body_id));

                    self.write_block(
                        body_id,
                        body,
                        Some(continuing_id),
                        LoopContext {
                            continuing_id: Some(continuing_id),
                            break_id: Some(merge_id),
                        },
                    )?;

                    self.write_block(
                        continuing_id,
                        continuing,
                        Some(preamble_id),
                        LoopContext {
                            continuing_id: None,
                            break_id: Some(merge_id),
                        },
                    )?;

                    block = Block::new(merge_id);
                }
                crate::Statement::Break => {
                    self.function
                        .consume(block, Instruction::branch(loop_context.break_id.unwrap()));
                    return Ok(());
                }
                crate::Statement::Continue => {
                    self.function.consume(
                        block,
                        Instruction::branch(loop_context.continuing_id.unwrap()),
                    );
                    return Ok(());
                }
                crate::Statement::Return { value: Some(value) } => {
                    let value_id = self.cached[value];
                    let instruction = match self.function.entry_point_context {
                        // If this is an entry point, and we need to return anything,
                        // let's instead store the output variables and return `void`.
                        Some(ref context) => {
                            self.writer.write_entry_point_return(
                                value_id,
                                self.ir_function.result.as_ref().unwrap(),
                                &context.results,
                                &mut block.body,
                            )?;
                            Instruction::return_void()
                        }
                        None => Instruction::return_value(value_id),
                    };
                    self.function.consume(block, instruction);
                    return Ok(());
                }
                crate::Statement::Return { value: None } => {
                    self.function.consume(block, Instruction::return_void());
                    return Ok(());
                }
                crate::Statement::Kill => {
                    self.function.consume(block, Instruction::kill());
                    return Ok(());
                }
                crate::Statement::Barrier(flags) => {
                    let memory_scope = if flags.contains(crate::Barrier::STORAGE) {
                        spirv::Scope::Device
                    } else {
                        spirv::Scope::Workgroup
                    };
                    let mut semantics = spirv::MemorySemantics::ACQUIRE_RELEASE;
                    semantics.set(
                        spirv::MemorySemantics::UNIFORM_MEMORY,
                        flags.contains(crate::Barrier::STORAGE),
                    );
                    semantics.set(
                        spirv::MemorySemantics::WORKGROUP_MEMORY,
                        flags.contains(crate::Barrier::WORK_GROUP),
                    );
                    let exec_scope_id = self.get_index_constant(spirv::Scope::Workgroup as u32)?;
                    let mem_scope_id = self.get_index_constant(memory_scope as u32)?;
                    let semantics_id = self.get_index_constant(semantics.bits())?;
                    block.body.push(Instruction::control_barrier(
                        exec_scope_id,
                        mem_scope_id,
                        semantics_id,
                    ));
                }
                crate::Statement::Store { pointer, value } => {
                    let value_id = self.cached[value];
                    match self.write_expression_pointer(pointer, &mut block)? {
                        ExpressionPointer::Ready { pointer_id } => {
                            block
                                .body
                                .push(Instruction::store(pointer_id, value_id, None));
                        }
                        ExpressionPointer::Conditional { condition, access } => {
                            let merge_block = self.gen_id();
                            let in_bounds_block = self.gen_id();

                            // Emit the conditional branch.
                            block.body.push(Instruction::selection_merge(
                                merge_block,
                                spirv::SelectionControl::NONE,
                            ));
                            self.function.consume(
                                std::mem::replace(&mut block, Block::new(in_bounds_block)),
                                Instruction::branch_conditional(
                                    condition,
                                    in_bounds_block,
                                    merge_block,
                                ),
                            );

                            // The in-bounds path. Perform the access and the store.
                            let pointer_id = access.result_id.unwrap();
                            block.body.push(access);
                            block
                                .body
                                .push(Instruction::store(pointer_id, value_id, None));

                            // Finish the in-bounds block and start the merge block. This
                            // is the block we'll leave current on return.
                            self.function.consume(
                                std::mem::replace(&mut block, Block::new(merge_block)),
                                Instruction::branch(merge_block),
                            );
                        }
                    };
                }
                crate::Statement::ImageStore {
                    image,
                    coordinate,
                    array_index,
                    value,
                } => {
                    let image_id = self.get_image_id(image);
                    let coordinate_id =
                        self.write_texture_coordinates(coordinate, array_index, &mut block)?;
                    let value_id = self.cached[value];

                    block
                        .body
                        .push(Instruction::image_write(image_id, coordinate_id, value_id));
                }
                crate::Statement::Call {
                    function: local_function,
                    ref arguments,
                    result,
                } => {
                    let id = self.gen_id();
                    self.temp_list.clear();
                    for &argument in arguments {
                        self.temp_list.push(self.cached[argument]);
                    }

                    let type_id = match result {
                        Some(expr) => {
                            self.cached[expr] = id;
                            self.writer.lookup_function_call.insert(expr, id);
                            let ty_handle = self.ir_module.functions[local_function]
                                .result
                                .as_ref()
                                .unwrap()
                                .ty;
                            self.get_type_id(LookupType::Handle(ty_handle))?
                        }
                        None => self.writer.void_type,
                    };

                    block.body.push(Instruction::function_call(
                        type_id,
                        id,
                        self.writer.lookup_function[&local_function],
                        &self.temp_list,
                    ));
                }
            }
        }

        let termination = match exit_id {
            Some(id) => Instruction::branch(id),
            // This can happen if the last branch had all the paths
            // leading out of the graph (i.e. returning).
            // Or it may be the end of the self.function.
            None => match self.ir_function.result {
                Some(ref result) if self.function.entry_point_context.is_none() => {
                    let type_id = self.get_type_id(LookupType::Handle(result.ty))?;
                    let null_id = self.writer.write_constant_null(type_id);
                    Instruction::return_value(null_id)
                }
                _ => Instruction::return_void(),
            },
        };

        self.function.consume(block, termination);
        Ok(())
    }
}
