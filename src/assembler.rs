use definition::Definition;
use rule::{Rule, PatternSegment};
use util::bitvec::BitVec;
use util::error::Error;
use util::expression::{Expression, ExpressionName, ExpressionValue};
use util::integer::Integer;
use util::label::{LabelManager, LabelContext};
use util::misc;
use util::parser::Parser;
use util::tokenizer;
use util::tokenizer::Span;
use std::path::PathBuf;


/// Holds intermediate information during assembly.
struct Assembler<'def>
{
	def: &'def Definition,
	cur_address: usize,
	cur_output: usize,
	labels: LabelManager,
	unresolved_instructions: Vec<Instruction>,
	unresolved_expressions: Vec<UnresolvedExpression>,
	
	output_bits: BitVec
}


/// Represents a parsed instruction with a matched rule.
/// Includes the context in which it appeared in the
/// source-code. Also includes full argument expressions
/// as seen in the source-code.
struct Instruction
{
	rule_index: usize,
	label_ctx: LabelContext,
	address: usize,
	output: usize,
	arguments: Vec<Expression>
}


/// Represents an unresolved expression in a data
/// directive. Includes the context in which it 
/// appeared in the source-code.
struct UnresolvedExpression
{
	expr: Expression,
	label_ctx: LabelContext,
	address: usize,
	output: usize,
	data_width: usize
}


/// Main interface to the assembly process.
pub fn assemble(def: &Definition, src_filename: &str, src: &[char]) -> Result<BitVec, Error>
{
	// Prepare an assembler state.
	let mut assembler = Assembler
	{
		def: def,
		cur_address: 0,
		cur_output: 0,
		labels: LabelManager::new(),
		unresolved_instructions: Vec::new(),
		unresolved_expressions: Vec::new(),
		output_bits: BitVec::new()
	};
	
	
	// == First-pass ==
	
	// Parse the main file.
	try!(assembler.parse_file(src_filename, src));	
	
	
	// == Second-pass ==
	
	// Resolve remaining instructions.
	let instrs: Vec<_> = assembler.unresolved_instructions.drain(..).collect();
	for instr in instrs
	{
		try!(assembler.resolve_instruction(&instr));
	}
	
	// Resolve remaining expressions in literals.
	let exprs: Vec<_> = assembler.unresolved_expressions.drain(..).collect();
	for expr in exprs
	{
		match try!(assembler.resolve_expr(&expr.expr, expr.label_ctx, expr.address))
		{
			ExpressionValue::Integer(ref integer) =>
				assembler.output_aligned_at(expr.output, &integer.slice(expr.data_width - 1, 0)),
				
			_ => return Err(Error::new_with_span("invalid expression", expr.expr.span.clone()))
		}
	}
	
	// Return output bits.
	Ok(assembler.output_bits)
}


impl<'def> Assembler<'def>
{
	/// Main parsing function.
	/// Reads source-code lines and decides how to decode them.
	fn parse_file(&mut self, src_filename: &str, src: &[char]) -> Result<(), Error>
	{
		let tokens = tokenizer::tokenize(src_filename, src);
		let mut parser = Parser::new(src_filename, &tokens);
		
		while !parser.is_over()
		{
			if parser.current().is_operator(".")
				{ try!(self.parse_directive(&mut parser)); }
				
			else if parser.current().is_identifier() && parser.next(1).is_operator("=")
				{ try!(self.parse_global_constant(&mut parser)); }
				
			else if parser.current().is_identifier() && parser.next(1).is_operator(":")
				{ try!(self.parse_global_label(&mut parser)); }
				
			else if parser.current().is_operator("'") && parser.next(1).is_identifier() && parser.next(2).is_operator(":")
				{ try!(self.parse_local_label(&mut parser)); }
				
			else
				{ try!(self.parse_instruction(&mut parser)); }
		}
		
		Ok(())
	}


	fn parse_directive(&mut self, parser: &mut Parser) -> Result<(), Error>
	{
		try!(parser.expect_operator("."));
		let (directive, directive_span) = try!(parser.expect_identifier());
		
		// If the directive starts with a 'd', it might
		// be a data directive.
		if directive.chars().next() == Some('d')
		{
			// Try to parse a number after the 'd'.
			match usize::from_str_radix(&directive[1..], 10)
			{
				Ok(data_width) =>
				{
					// If there was a valid number after the 'd',
					// check for validity, and then
					// call a more specialized function.
					if data_width % self.def.align_bits != 0
					{
						return Err(Error::new_with_span(
							format!("data directive is not aligned to `{}` bits", self.def.align_bits),
							directive_span));
					}
				
					if data_width > 63
					{
						return Err(Error::new_with_span(
							"data directive bit width is currently not supported",
							directive_span));
					}
					
					return self.parse_data_directive(parser, data_width);
				}
				
				Err(_) =>
				{
					// If there was an invalid number after the 'd',
					// fallthrough to the directive-matcher below.
				}
			}
		}
		
		// Parse text-only directives.
		match directive.as_ref()
		{
			"address" =>
			{
				let expr = try!(Expression::new_by_parsing(parser));
				let value = try!(self.resolve_expr_current(&expr));
				self.cur_address = try!(self.extract_integer(value, &expr.span));
			}
			
			"output" => 
			{
				let expr = try!(Expression::new_by_parsing(parser));
				let value = try!(self.resolve_expr_current(&expr));
				self.cur_output = try!(self.extract_integer(value, &expr.span));
			}
			
			"res" => 
			{
				let expr = try!(Expression::new_by_parsing(parser));
				let value = try!(self.resolve_expr_current(&expr));
				let bits = self.def.align_bits * try!(self.extract_integer(value, &expr.span));
				self.advance_address(bits);
			}
			
			"include" =>
			{
				let include_filename = try!(parser.expect_string()).string().clone();
				let mut cur_path = PathBuf::from(parser.get_filename());
				cur_path.set_file_name(&include_filename);
				let include_chars = misc::read_file(&cur_path);
				try!(self.parse_file(&cur_path.to_string_lossy().into_owned(), &include_chars));
			}
			
			"includebin" => 
			{
				let include_filename = try!(parser.expect_string()).string().clone();
				let mut cur_path = PathBuf::from(parser.get_filename());
				cur_path.set_file_name(&include_filename);
				//let include_bitvec = BitVec::new_from_bytes(&misc::read_file_bytes(&cur_path));
				//self.output_aligned(&include_bitvec);
			}
			
			_ => return Err(Error::new_with_span(format!("unknown directive `{}`", directive), directive_span))
		}
		
		try!(parser.expect_linebreak_or_end());
		Ok(())
	}


	fn parse_data_directive(&mut self, parser: &mut Parser, data_size: usize) -> Result<(), Error>
	{
		// Parse expressions until there isn't a comma.
		loop
		{
			let expr = try!(Expression::new_by_parsing(parser));
			
			try!(self.output_expression(expr, data_size));
			
			if !parser.match_operator(",")
				{ break; }
		}
		
		try!(parser.expect_linebreak_or_end());
		Ok(())
	}


	fn parse_global_constant(&mut self, parser: &mut Parser) -> Result<(), Error>
	{
		let (label, label_span) = try!(parser.expect_identifier());
		try!(parser.expect_operator("="));
		
		// Check for duplicate global labels.
		if self.labels.does_global_exist(&label)
			{ return Err(Error::new_with_span(format!("duplicate global label `{}`", label), label_span)); }
		
		// Resolve constant value.
		let expr = try!(Expression::new_by_parsing(parser));
		let value = try!(self.resolve_expr_current(&expr));
		
		// Store it.
		self.labels.add_global(label, value);
		
		try!(parser.expect_linebreak_or_end());
		Ok(())
	}


	fn parse_global_label(&mut self, parser: &mut Parser) -> Result<(), Error>
	{
		let (label, label_span) = try!(parser.expect_identifier());
		try!(parser.expect_operator(":"));
		
		// Check for duplicate global labels.
		if self.labels.does_global_exist(&label)
			{ return Err(Error::new_with_span(format!("duplicate global label `{}`", label), label_span)); }
		
		// Store as current address.
		self.labels.add_global(
			label,
			ExpressionValue::Integer(Integer::new(self.cur_address as i64)));
		
		try!(parser.expect_linebreak_or_end());
		Ok(())
	}


	fn parse_local_label(&mut self, parser: &mut Parser) -> Result<(), Error>
	{
		try!(parser.expect_operator("'"));
		let (label, label_span) = try!(parser.expect_identifier());
		try!(parser.expect_operator(":"));
		
		let local_ctx = self.labels.get_cur_context();
		
		// Check for duplicate local labels within the same context.
		if self.labels.does_local_exist(local_ctx, &label)
			{ return Err(Error::new_with_span(format!("duplicate local label `{}`", label), label_span)); }
		
		// Store as current address.
		self.labels.add_local(
			local_ctx,
			label,
			ExpressionValue::Integer(Integer::new(self.cur_address as i64)));
		
		try!(parser.expect_linebreak_or_end());
		Ok(())
	}


	fn parse_instruction<'p, 'f, 'tok>(&mut self, parser: &'p mut Parser<'f, 'tok>) -> Result<(), Error>
	{
		let mut maybe_match = None;
		let instr_span = parser.current().span.clone();

		// Try every rule from the definition.
		for rule_index in 0..self.def.rules.len()
		{
			// Clone the parser, to maintain the current one stationary.
			// If the rule doesn't match, the clone is simply discarded.
			// If it does match, the clone will become the main parser.
			let mut rule_parser = parser.clone_from_current();
			
			match try!(self.try_match_rule(&mut rule_parser, rule_index))
			{
				Some(instr) =>
				{
					let can_resolve = try!(self.can_resolve_instruction(&instr));
					
					maybe_match = Some((instr, rule_parser));
					
					if can_resolve
						{ break; }
				}
				
				None =>
				{
					// If the rule didn't match, just continue trying
					// with the next rule.
				}
			}
		}
		
		// Check whether there was a rule match.
		match maybe_match
		{
			Some((instr, new_parser)) =>
			{
				*parser = new_parser;
				try!(self.output_instruction(instr));
			}
			
			None => return Err(Error::new_with_span("no match found for instruction", instr_span))
		}
		
		try!(parser.expect_linebreak_or_end());
		Ok(())
	}


	fn try_match_rule(&mut self, parser: &mut Parser, rule_index: usize) -> Result<Option<Instruction>, Error>
	{
		let rule = &self.def.rules[rule_index];
		
		let mut instr = Instruction
		{
			label_ctx: self.labels.get_cur_context(),
			rule_index: rule_index,
			address: self.cur_address,
			output: self.cur_output,
			arguments: Vec::new()
		};
		
		// Try matching against every segment in the rule pattern.
		for segment in rule.pattern_segments.iter()
		{
			match segment
			{
				&PatternSegment::Exact(ref chars) =>
				{
					if parser.current().is_identifier() && parser.current().identifier() == chars
						{ parser.advance(); }
						
					else if parser.current().is_operator(&chars)
						{ parser.advance(); }
						
					else
						{ return Ok(None); }
				}
				
				&PatternSegment::Parameter(param_index) =>
				{
					let expr = try!(Expression::new_by_parsing(parser));
					
					if !rule.get_parameter_allow_unresolved(param_index)
					{
						let label_ctx = self.labels.get_cur_context();
						
						if !try!(self.can_resolve_expr(&expr, label_ctx))
							{ return Ok(None); }
						
						match rule.get_parameter_constraint(param_index)
						{
							&None => { },
							
							&Some(ref constraint) =>
							{
								let value = try!(self.resolve_expr(&expr, label_ctx, self.cur_address));
								
								if !try!(self.check_constraint(&constraint, &value, self.cur_address))
									{ return Ok(None); }
							}
						}
					}
					
					instr.arguments.push(expr);
				}
			}
		}
		
		Ok(Some(instr))
	}


	fn advance_address(&mut self, bit_num: usize)
	{
		assert!(bit_num % self.def.align_bits == 0);
		let address_inc = bit_num / self.def.align_bits;
		self.cur_output += address_inc;
		self.cur_address += address_inc;
	}
	

	fn output_aligned(&mut self, value: &Integer)
	{
		let aligned_index = self.cur_output * self.def.align_bits;
		self.output_bits.set(aligned_index, value);
		self.advance_address(value.get_width());
	}
	
	
	fn output_aligned_at(&mut self, index: usize, value: &Integer)
	{
		let aligned_index = index * self.def.align_bits;
		self.output_bits.set(aligned_index, value);
	}
	
	
	fn output_expression(&mut self, expr: Expression, data_width: usize) -> Result<(), Error>
	{
		let label_ctx = self.labels.get_cur_context();
		
		// Try resolving the expression immediately.
		if try!(self.can_resolve_expr(&expr, label_ctx))
		{
			match try!(self.resolve_expr(&expr, label_ctx, self.cur_address))		
			{
				ExpressionValue::Integer(integer) =>
					self.output_aligned(&integer.slice(data_width - 1, 0)),
				
				_ => return Err(Error::new_with_span("invalid expression type", expr.span.clone()))
			}
		}
		
		// If unresolvable now, store it to be resolved
		// on the second-pass.
		else
		{
			self.unresolved_expressions.push(UnresolvedExpression
			{
				expr: expr,
				label_ctx: label_ctx,
				address: self.cur_address,
				output: self.cur_output,
				data_width: data_width
			});
			
			self.advance_address(data_width);
		}
		
		Ok(())
	}
	
	
	fn output_instruction(&mut self, instr: Instruction) -> Result<(), Error>
	{
		let rule = &self.def.rules[instr.rule_index];
		
		self.advance_address(rule.production_bit_num);
		
		// Try resolving the instruction's arguments immediately.
		if try!(self.can_resolve_instruction(&instr))
			{ try!(self.resolve_instruction(&instr)); }
		
		// If unresolvable now, store it to be resolved
		// on the second-pass.
		else
			{ self.unresolved_instructions.push(instr); }
			
		Ok(())
	}


	fn can_resolve_instruction(&self, instr: &Instruction) -> Result<bool, Error>
	{
		for expr in instr.arguments.iter()
		{
			if !try!(self.can_resolve_expr(expr, instr.label_ctx))
				{ return Ok(false); }
		}
		
		Ok(true)
	}


	fn resolve_instruction(&mut self, instr: &Instruction) -> Result<(), Error>
	{
		let rule = &self.def.rules[instr.rule_index];
		
		let mut width = 0;
		for expr in rule.production_segments.iter()
		{
			match try!(self.resolve_production(rule, instr, expr))
			{
				ExpressionValue::Integer(integer) =>
				{
					self.output_aligned_at(instr.output + width, &integer);
					width += integer.get_width() / self.def.align_bits;
				}
				
				_ => return Err(Error::new_with_span("invalid production expression type", expr.span.clone()))
			}
		}
		
		Ok(())
	}
	
	
	fn can_resolve_expr(&self, expr: &Expression, ctx: LabelContext) -> Result<bool, Error>
	{
		expr.can_resolve(&|expr_name, _|
		{
			match expr_name
			{
				ExpressionName::GlobalVariable(name) => Ok(name == "pc" || self.labels.does_global_exist(name)),
				ExpressionName::LocalVariable(name) => Ok(self.labels.does_local_exist(ctx, name))
			}
		})
	}
	
	
	fn resolve_expr(&self, expr: &Expression, ctx: LabelContext, pc: usize) -> Result<ExpressionValue, Error>
	{
		expr.resolve(&|name_kind, name_span|
		{
			match name_kind
			{
				ExpressionName::GlobalVariable(name) => match name
				{
					"pc" => Ok(ExpressionValue::Integer(Integer::new(pc as i64))),
					
					name => match self.labels.get_global(name)
					{
						Some(value) => Ok(value.clone()),
						None => Err(Error::new_with_span(format!("unknown `{}`", name), name_span.clone()))
					}
				},
				
				ExpressionName::LocalVariable(name) => match self.labels.get_local(ctx, name)
				{
					Some(value) => Ok(value.clone()),
					None => Err(Error::new_with_span(format!("unknown local `{}`", name), name_span.clone()))
				}
			}
		})
	}
	
	
	fn resolve_expr_current(&self, expr: &Expression) -> Result<ExpressionValue, Error>
	{
		let label_ctx = self.labels.get_cur_context();
		let address = self.cur_address;
		
		self.resolve_expr(expr, label_ctx, address)
	}
	
	
	fn resolve_production(&self, rule: &Rule, instr: &Instruction, expr: &Expression) -> Result<ExpressionValue, Error>
	{
		expr.resolve(&|param_kind, _|
		{
			match param_kind
			{
				ExpressionName::GlobalVariable(name) => match name
				{
					"pc" => Ok(ExpressionValue::Integer(Integer::new(instr.address as i64))),
					
					name =>
					{
						let param_index = rule.get_parameter(name).unwrap();
						let arg_expr = &instr.arguments[param_index];
						
						let arg_value = try!(self.resolve_expr(arg_expr, instr.label_ctx, instr.address));
						
						match rule.get_parameter_constraint(param_index)
						{
							&None => { },
							
							&Some(ref constraint_expr) =>
							{
								if !try!(self.check_constraint(&constraint_expr, &arg_value, instr.address))
									{ return Err(Error::new_with_span("parameter constraint not satisfied", arg_expr.span.clone())); }
							}
						}
						
						Ok(arg_value)
					}
				},
				
				_ => unreachable!()
			}
		})
	}
	
	
	fn check_constraint(&self, constraint: &Expression, argument: &ExpressionValue, address: usize) -> Result<bool, Error>
	{
		let constraint_result = try!(constraint.resolve(&|expr_name, _|
		{
			match expr_name
			{
				ExpressionName::GlobalVariable(name) => match name
				{
					"_" => Ok(argument.clone()),
					
					"pc" => Ok(ExpressionValue::Integer(Integer::new(address as i64))),
					
					_ => unreachable!()
				},
						
				_ => unreachable!()
			}
		}));
		
		match constraint_result
		{
			ExpressionValue::Boolean(b) => Ok(b),					
			_ => Err(Error::new_with_span("invalid constraint expression type", constraint.span.clone()))
		}
	}
	
	
	fn extract_integer(&self, value: ExpressionValue, span: &Span) -> Result<usize, Error>
	{
		match value
		{
			ExpressionValue::Integer(integer) => Ok(integer.value as usize),
			_ => Err(Error::new_with_span("expected integer value", span.clone()))
		}
	}
}