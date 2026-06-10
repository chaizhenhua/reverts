
				@dec class Class {
				}
				class ClassMethod {
					@dec foo() {}
				}
				class ClassField {
					@dec foo = 123
					@dec bar
				}
				class ClassAccessor {
					@dec accessor foo = 123
					@dec accessor bar
				}
				new Class
				new ClassMethod
				new ClassField
				new ClassAccessor
			